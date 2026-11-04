#[path = "src/app_update/build_key.rs"]
mod build_update_key;

fn main() {
    // Tauri forwards TAURI_* variables through mobile IDE builds.
    println!("cargo:rerun-if-env-changed=TAURI_RISUNEST_UPDATE_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=RISUNEST_UPDATE_PUBLIC_KEY");
    println!("cargo:rerun-if-changed=src/app_update/build_key.rs");
    let forwarded_key = std::env::var("TAURI_RISUNEST_UPDATE_PUBLIC_KEY").ok();
    let legacy_key = std::env::var("RISUNEST_UPDATE_PUBLIC_KEY").ok();
    let public_key = build_update_key::resolve_update_public_key(
        forwarded_key.as_deref(),
        legacy_key.as_deref(),
        std::env::var_os("CARGO_FEATURE_OFFICIAL_DISTRIBUTION").is_some(),
    )
    .expect("invalid updater build configuration");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("missing OUT_DIR"));
    std::fs::write(
        output.join("update_public_key.rs"),
        format!("const PUBLIC_KEY: &str = {public_key:?};\n"),
    )
    .expect("failed to write updater public key");

    if matches!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("macos") | Ok("ios")) {
        println!("cargo:rerun-if-changed=src/apple_startup_log.c");
        cc::Build::new().file("src/apple_startup_log.c").compile("risunest_startup_log");
        println!("cargo:rerun-if-changed=src/cleanup_webview.m");
        cc::Build::new().file("src/cleanup_webview.m").flag("-fobjc-arc")
            .flag("-fblocks").compile("risunest_cleanup_webview");
        println!("cargo:rerun-if-changed=src/network_log_backup.m");
        cc::Build::new().file("src/network_log_backup.m").flag("-fobjc-arc").compile("risunest_network_log_backup");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=WebKit");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=src/macos_termination.m");
        cc::Build::new().file("src/macos_termination.m").compile("risunest_termination");
        println!("cargo:rustc-link-lib=framework=AppKit");
    }
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    let attributes = if windows_msvc {
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest())
    } else {
        tauri_build::Attributes::new()
    };

    tauri_build::try_build(attributes).expect("failed to run Tauri build script");

    // Non-dev builds embed `frontendDist`. Rustc tracks only the files it embedded, so a
    // file added to the frontend build alone would otherwise leave the crate fresh.
    if !tauri_build::is_dev() && std::path::Path::new("../dist").exists() {
        println!("cargo:rerun-if-changed=../dist");
    }

    if windows_msvc {
        let manifest = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR")
                .expect("Cargo did not provide CARGO_MANIFEST_DIR"),
        )
        .join("windows-app-manifest.xml");

        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
}
