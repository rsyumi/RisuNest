use risunest_release_update::{
    Architecture, OperatingSystem, PackageFormat, PackageRequest, Product, Variant,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum InstallStrategy {
    SelfInstall,
    StageDeb,
    OpenLink,
    Disabled,
}

#[derive(Clone, Debug)]
pub(crate) struct PlatformSelection {
    pub strategy: InstallStrategy,
    pub request: Option<PackageRequest>,
    pub target: Option<String>,
}

pub(crate) fn current_selection() -> PlatformSelection {
    #[cfg(target_os = "macos")]
    if std::env::current_exe()
        .ok()
        .and_then(|path| tauri_plugin_updater::extract_path_from_executable(&path).ok())
        .is_none()
    {
        return disabled();
    }
    selection(
        std::env::consts::OS,
        std::env::consts::ARCH,
        tauri::utils::platform::bundle_type()
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
    )
}

pub(crate) fn selection(os: &str, arch: &str, bundle_type: Option<&str>) -> PlatformSelection {
    let Some(arch) = architecture(arch) else {
        return disabled();
    };
    match os {
        "windows" => match bundle_type {
            Some("nsis") => exact(
                InstallStrategy::SelfInstall,
                OperatingSystem::Windows,
                arch,
                PackageFormat::Nsis,
                Some(format!("windows-{arch}-nsis")),
            ),
            None => exact(
                InstallStrategy::OpenLink,
                OperatingSystem::Windows,
                arch,
                PackageFormat::Zip,
                None,
            ),
            _ => disabled(),
        },
        "linux" => match bundle_type {
            Some("appimage") => exact(
                InstallStrategy::SelfInstall,
                OperatingSystem::Linux,
                arch,
                PackageFormat::AppImage,
                Some(format!("linux-{arch}-appimage")),
            ),
            Some("deb") => exact(
                InstallStrategy::StageDeb,
                OperatingSystem::Linux,
                arch,
                PackageFormat::Deb,
                Some(format!("linux-{arch}-deb")),
            ),
            _ => disabled(),
        },
        "macos" => exact(
            InstallStrategy::SelfInstall,
            OperatingSystem::Darwin,
            arch,
            PackageFormat::AppTarGz,
            Some(format!("darwin-{arch}-app")),
        ),
        "android" if arch == Architecture::Aarch64 => exact(
            InstallStrategy::OpenLink,
            OperatingSystem::Android,
            arch,
            PackageFormat::Apk,
            None,
        ),
        "ios" if arch == Architecture::Aarch64 => exact(
            InstallStrategy::OpenLink,
            OperatingSystem::Ios,
            arch,
            PackageFormat::Ipa,
            None,
        ),
        _ => disabled(),
    }
}

fn exact(
    strategy: InstallStrategy,
    os: OperatingSystem,
    arch: Architecture,
    format: PackageFormat,
    target: Option<String>,
) -> PlatformSelection {
    PlatformSelection {
        strategy,
        request: Some(PackageRequest {
            product: Product::App,
            variant: if matches!(os, OperatingSystem::Android | OperatingSystem::Ios) {
                Variant::Mobile
            } else {
                Variant::Desktop
            },
            os,
            arch,
            format,
        }),
        target,
    }
}

fn architecture(value: &str) -> Option<Architecture> {
    match value {
        "x86_64" => Some(Architecture::X86_64),
        "aarch64" => Some(Architecture::Aarch64),
        _ => None,
    }
}

fn disabled() -> PlatformSelection {
    PlatformSelection {
        strategy: InstallStrategy::Disabled,
        request: None,
        target: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_install_types_never_fall_back_to_another_package() {
        assert_eq!(
            selection("windows", "x86_64", Some("nsis")).strategy,
            InstallStrategy::SelfInstall
        );
        assert_eq!(
            selection("windows", "x86_64", None).request.unwrap().format,
            PackageFormat::Zip
        );
        assert_eq!(
            selection("linux", "aarch64", Some("deb")).strategy,
            InstallStrategy::StageDeb
        );
        assert_eq!(
            selection("linux", "aarch64", Some("rpm")).strategy,
            InstallStrategy::Disabled
        );
        assert_eq!(
            selection("windows", "x86", Some("nsis")).strategy,
            InstallStrategy::Disabled
        );
    }
}
