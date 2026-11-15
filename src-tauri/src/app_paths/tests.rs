use super::{
    default_install_dirs, is_data_root_entry, overlaps, platform_store_root, relocated_external_storage_path,
    resolve_platform_root, AppPaths, DesktopProfiles, Hives, Integration, LinkRefusal, Platform, ProfileBases,
};
use crate::launch_profile::ProfileName;
use crate::asset_repository::{ExactObjectUnlink, PayloadCas};
use std::{fs, io::ErrorKind, path::Path, path::PathBuf};

const IDENTIFIER: &str = "io.github.rsyumi.risunest";

/// An absolute synthetic base on any host, so one table pins every platform row.
fn base(name: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:\\synthetic\\{name}"))
    } else {
        PathBuf::from(format!("/synthetic/{name}"))
    }
}

fn hives() -> Hives {
    Hives {
        roaming: Some(base("roaming")),
        local: Some(base("local")),
        config: Some(base("config")),
        cache: Some(base("cache")),
        home: Some(base("home")),
        os_data: Some(base("sandbox")),
        os_cache: Some(base("sandbox/cache")),
        os_documents: None,
    }
}

fn layout(platform: Platform) -> AppPaths {
    AppPaths::layout(platform, &platform_hives(platform), IDENTIFIER, false).expect("resolve synthetic layout")
}

fn platform_hives(platform: Platform) -> Hives {
    let mut result = hives();
    if platform == Platform::Ios {
        result.os_data = Some(base("sandbox/Library/Application Support").join(IDENTIFIER));
        result.os_cache = Some(base("sandbox/Library/Caches").join(IDENTIFIER));
        result.os_documents = Some(base("sandbox/Documents"));
    }
    result
}

#[test]
fn every_platform_row_matches_the_recorded_matrix() {
    let windows = layout(Platform::Windows);
    assert_eq!(windows.data, base("local").join("RisuNestData"));
    assert_eq!(
        windows.webview,
        Some(base("local").join("RisuNestWebViewData"))
    );
    assert_eq!(windows.cache, base("local").join("RisuNestCache"));
    assert_eq!(
        windows.cleanup_control,
        base("local").join("RisuNest-cleanup")
    );
    assert_eq!(windows.integration, None);

    let macos = layout(Platform::MacOs);
    assert_eq!(macos.data, base("roaming").join("RisuNest"));
    assert_eq!(macos.webview, None);
    assert_eq!(macos.cache, base("cache").join("RisuNest"));
    assert_eq!(
        macos.cleanup_control,
        base("roaming").join("RisuNest-cleanup")
    );
    assert!(macos
        .tauri_derived
        .contains(&base("home").join("Library/Logs").join(IDENTIFIER)));

    let linux = layout(Platform::Linux);
    assert_eq!(linux.data, base("roaming").join("risunest"));
    assert_eq!(linux.webview, Some(base("roaming").join("risunest-webview")));
    assert_eq!(linux.cache, base("cache").join("risunest"));
    assert_eq!(
        linux.cleanup_control,
        base("roaming").join("risunest-cleanup")
    );
    assert_eq!(
        linux.integration,
        Some(Integration {
            data_home: base("roaming"),
            config_home: base("config"),
        })
    );

    for platform in [Platform::Android, Platform::Ios] {
        let mobile = layout(platform);
        let expected = if platform == Platform::Android { base("sandbox/files") } else { base("sandbox/Library/Application Support").join(IDENTIFIER) };
        assert_eq!(mobile.data, expected);
        assert_eq!(mobile.webview, None);
        let expected_cache = if platform == Platform::Android { base("sandbox/cache") } else { base("sandbox/Library/Caches").join(IDENTIFIER) };
        assert_eq!(mobile.cache, expected_cache);
        assert_eq!(
            mobile.cleanup_control,
            expected.parent().unwrap().join("risunest-cleanup")
        );
        if platform == Platform::Android { assert!(mobile.tauri_derived.is_empty()); }
        else { assert_eq!(mobile.tauri_derived, vec![base("sandbox/Documents/Inbox")]); }
    }

    for platform in [
        Platform::Windows,
        Platform::MacOs,
        Platform::Linux,
        Platform::Android,
        Platform::Ios,
    ] {
        let paths = layout(platform);
        assert_eq!(paths.logs, paths.data.join("logs"));
        assert_eq!(paths.install, None);
    }
}

fn profiles(platform: Platform, hives: Hives, agent: bool) -> DesktopProfiles {
    DesktopProfiles { platform, hives, identifier: IDENTIFIER.to_owned(), agent, install: None }
}

fn name(value: &str) -> ProfileName {
    ProfileName::parse(value).unwrap()
}

#[test]
fn named_profiles_match_the_recorded_matrix() {
    let test = name("test");
    let windows = profiles(Platform::Windows, hives(), false).manifest(Some(&test)).unwrap();
    let directory = base("local").join("RisuNestProfiles").join("test");
    assert_eq!(windows.data, directory.join("data"));
    assert_eq!(windows.logs, directory.join("data").join("logs"));
    assert_eq!(windows.webview, Some(directory.join("webview")));
    assert_eq!(windows.cache, directory.join("cache"));
    assert_eq!(windows.cleanup_control, directory.join("cleanup"));

    let macos = profiles(Platform::MacOs, hives(), false).manifest(Some(&test)).unwrap();
    let directory = base("roaming").join("RisuNestProfiles").join("test");
    assert_eq!(macos.data, directory.join("data"));
    assert_eq!(macos.webview, None);
    assert_eq!(macos.cache, base("cache").join("RisuNestProfiles").join("test"));
    assert_eq!(macos.cleanup_control, directory.join("cleanup"));

    let linux = profiles(Platform::Linux, hives(), false).manifest(Some(&test)).unwrap();
    let directory = base("roaming").join("risunest-profiles").join("test");
    assert_eq!(linux.data, directory.join("data"));
    assert_eq!(linux.webview, Some(directory.join("webview")));
    assert_eq!(linux.cache, base("cache").join("risunest-profiles").join("test"));
    assert_eq!(linux.cleanup_control, directory.join("cleanup"));

    for paths in [windows, macos, linux] {
        assert!(paths.tauri_derived.is_empty());
        assert_eq!(paths.integration, None);
        assert_eq!(paths.install, None);
    }
    for platform in [Platform::Windows, Platform::MacOs, Platform::Linux] {
        assert_eq!(profiles(platform, hives(), false).manifest(None).unwrap(), layout(platform));
        let agent = profiles(platform, hives(), true).bases().unwrap();
        let product = profiles(platform, hives(), false).bases().unwrap();
        assert_eq!(agent.profiles, product.profiles.with_file_name(format!(
            "{}-agent", product.profiles.file_name().unwrap().to_str().unwrap())));
        if product.caches.is_some() {
            assert_ne!(agent.caches, product.caches);
        }
    }
    assert_eq!(profiles(Platform::Windows, hives(), false).bases().unwrap(),
        ProfileBases { profiles: base("local").join("RisuNestProfiles"), caches: None });
    for platform in [Platform::Android, Platform::Ios] {
        assert!(profiles(platform, platform_hives(platform), false).manifest(Some(&test)).is_err());
    }
}

#[test]
fn no_profile_root_reaches_the_default_profile_or_another_profile() {
    for platform in [Platform::Windows, Platform::MacOs, Platform::Linux] {
        for agent in [false, true] {
            let profiles = profiles(platform, hives(), agent);
            let manifests = [None, Some(name("test")), Some(name("test-cleanup")), Some(name("tests"))]
                .map(|profile| profiles.manifest(profile.as_ref()).unwrap());
            for (index, left) in manifests.iter().enumerate() {
                for right in &manifests[index + 1..] {
                    for left_root in left.owned_roots().iter().chain([&left.cleanup_control]) {
                        for right_root in right.owned_roots().iter().chain([&right.cleanup_control]) {
                            assert!(!overlaps(left_root, right_root), "{platform:?} {} overlaps {}",
                                left_root.display(), right_root.display());
                        }
                    }
                }
            }
            let directories = profiles.directories(&name("test")).unwrap();
            let test = &manifests[1];
            for root in test.owned_roots().iter().chain([&test.cleanup_control]) {
                assert!(directories.iter().any(|directory| root.starts_with(directory)), "{}", root.display());
            }
        }
    }
}

#[test]
fn only_real_directories_with_valid_names_are_profiles() {
    let fixture = tempfile::tempdir().unwrap();
    let mut synthetic = hives();
    synthetic.roaming = Some(fixture.path().join("data-home"));
    synthetic.cache = Some(fixture.path().join("cache-home"));
    let linux = profiles(Platform::Linux, synthetic, false);
    assert_eq!(linux.names().unwrap(), Vec::<ProfileName>::new());
    let bases = linux.bases().unwrap();
    let caches = bases.caches.clone().unwrap();
    for directory in ["work", "Invalid", "has.dot", "default"] {
        fs::create_dir_all(bases.profiles.join(directory)).unwrap();
    }
    fs::create_dir_all(caches.join("cache-only")).unwrap();
    fs::create_dir_all(caches.join("work")).unwrap();
    fs::write(bases.profiles.join("file"), b"synthetic").unwrap();
    let outside = tempfile::tempdir().unwrap();
    link_directory(outside.path(), &bases.profiles.join("linked"));
    assert_eq!(linux.names().unwrap(), vec![name("cache-only"), name("work")]);
}

#[test]
fn android_owns_only_files_and_cache_inside_the_actual_data_dir() {
    let fixture = tempfile::tempdir().unwrap();
    let sandbox = fixture.path().join("user/10/synthetic.app");
    let mut hives = hives();
    hives.os_data = Some(sandbox.clone());
    hives.os_cache = Some(sandbox.join("cache"));
    let paths = AppPaths::layout(Platform::Android, &hives, IDENTIFIER, false).unwrap();
    assert_eq!(paths.data, sandbox.join("files"));
    assert_eq!(paths.cleanup_control, sandbox.join("risunest-cleanup"));
    let mut roots = paths.owned_roots();
    roots.sort();
    assert_eq!(roots, vec![sandbox.join("cache"), sandbox.join("files")]);
    for managed in ["shared_prefs", "databases", "app_webview", "code_cache", "lib"] {
        assert!(!roots.iter().any(|root| sandbox.join(managed).starts_with(root)));
    }
}

#[test]
fn ios_cleanup_owns_only_the_inbox_child_of_os_documents() {
    let paths = layout(Platform::Ios);
    let roots = paths.owned_roots();
    let documents = base("sandbox/Documents");
    assert!(roots.contains(&documents.join("Inbox")));
    for preserved in [documents.clone(), documents.join("export.risunest"), documents.join("exports")] {
        assert!(!roots.iter().any(|root| preserved.starts_with(root)));
    }
    let mut relocated = platform_hives(Platform::Ios);
    relocated.os_documents = Some(base("another-container/Documents"));
    let paths = AppPaths::layout(Platform::Ios, &relocated, IDENTIFIER, false).unwrap();
    assert_eq!(paths.tauri_derived, vec![base("another-container/Documents/Inbox")]);
}

#[test]
fn identifier_derived_directories_are_swept_but_are_never_the_store() {
    for platform in [Platform::Windows, Platform::MacOs, Platform::Linux] {
        let paths = layout(platform);
        assert!(!paths.tauri_derived.is_empty());
        for derived in &paths.tauri_derived {
            assert!(derived.ends_with(IDENTIFIER) || derived.ends_with(Path::new(IDENTIFIER)));
            assert_ne!(*derived, paths.data);
            assert!(!overlaps(derived, &paths.data));
        }
        assert!(paths.owned_roots().contains(&paths.data));
        assert!(paths.owned_roots().contains(&paths.cache));
    }
}

#[test]
fn no_owned_root_overlaps_the_default_installation_directory() {
    for platform in [Platform::Windows, Platform::MacOs, Platform::Linux] {
        let mut paths = layout(platform);
        let defaults = default_install_dirs(platform, &hives());
        assert!(
            !defaults.is_empty(),
            "{platform:?} has no recorded default installation directory"
        );
        for install in defaults {
            paths.install = Some(install.clone());
            assert_eq!(
                paths.install_conflict(),
                None,
                "{platform:?} root overlaps {}",
                install.display()
            );
        }
    }
}

#[test]
fn the_windows_store_is_a_sibling_of_the_installation_directory_not_a_child() {
    let paths = layout(Platform::Windows);
    let install = default_install_dirs(Platform::Windows, &hives())
        .pop()
        .expect("windows installation directory");
    assert_eq!(install, base("local").join("RisuNest"));
    // A string-prefix comparison would report this as nested.
    assert!(paths
        .data
        .to_string_lossy()
        .starts_with(&*install.to_string_lossy()));
    assert!(!overlaps(&paths.data, &install));
}

#[test]
fn a_root_that_holds_the_installation_directory_is_refused() {
    let mut paths = layout(Platform::Windows);
    paths.install = Some(paths.data.clone());
    assert_eq!(paths.install_conflict(), Some(paths.data.clone()));

    paths.install = Some(paths.data.join("nested"));
    assert_eq!(paths.install_conflict(), Some(paths.data.clone()));

    paths.install = Some(base("local"));
    assert!(paths.install_conflict().is_some());

    // The control directory is checked as well; it is deleted last but still
    // sits under a hive the installer could have been pointed at.
    let mut paths = layout(Platform::Windows);
    paths.install = Some(paths.cleanup_control.clone());
    assert_eq!(
        paths.install_conflict(),
        Some(paths.cleanup_control.clone())
    );
}

#[test]
fn an_agent_build_never_shares_a_desktop_root_with_the_installed_product() {
    for platform in [Platform::Windows, Platform::MacOs, Platform::Linux] {
        let installed = layout(platform);
        let agent = AppPaths::layout(platform, &hives(), IDENTIFIER, true).unwrap();
        assert_ne!(agent.data, installed.data);
        assert_ne!(agent.cleanup_control, installed.cleanup_control);
        assert_ne!(agent.cache, installed.cache);
        if installed.webview.is_some() {
            assert_ne!(agent.webview, installed.webview);
        }
        // The identifier-derived sweep roots carry no agent leaf, so an agent
        // build claims none of them rather than deleting the product's.
        assert!(agent.tauri_derived.is_empty());
        assert!(!installed.tauri_derived.is_empty());
        for root in agent.owned_roots() {
            assert!(
                !installed.owned_roots().contains(&root),
                "{} is shared with the installed product",
                root.display()
            );
        }
    }
}

#[test]
fn an_agent_build_keeps_the_mobile_sandbox_unchanged() {
    for platform in [Platform::Android, Platform::Ios] {
        assert_eq!(
            AppPaths::layout(platform, &platform_hives(platform), IDENTIFIER, true).unwrap(),
            layout(platform)
        );
    }
}

#[test]
fn a_missing_hive_or_invalid_identifier_is_an_error_not_a_guess() {
    let mut missing_documents = platform_hives(Platform::Ios);
    missing_documents.os_documents = None;
    assert!(AppPaths::layout(Platform::Ios, &missing_documents, IDENTIFIER, false).is_err());
    let mut missing = hives();
    missing.local = None;
    assert!(AppPaths::layout(Platform::Windows, &missing, IDENTIFIER, false).is_err());

    let mut relative = hives();
    relative.roaming = Some(PathBuf::from("relative"));
    assert!(AppPaths::layout(Platform::Linux, &relative, IDENTIFIER, false).is_err());

    for invalid in ["", "has space", "has/slash", "has\\slash"] {
        assert!(AppPaths::layout(Platform::Windows, &hives(), invalid, false).is_err());
    }
}

#[cfg(unix)]
pub(crate) fn link_directory(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create synthetic directory link");
}

#[cfg(windows)]
pub(crate) fn link_directory(target: &Path, link: &Path) {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("cmd")
        .creation_flags(0x08000000)
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("create synthetic directory junction");
    assert!(output.status.success(), "create junction: {output:?}");
}

#[test]
fn platform_parent_alias_supports_cas_write_read_and_delete() {
    let fixture = tempfile::tempdir().unwrap();
    let real_parent = fixture.path().join("data");
    let real_root = real_parent.join("synthetic.app");
    fs::create_dir_all(&real_root).unwrap();
    let alias_parent = fixture.path().join("user-0");
    link_directory(&real_parent, &alias_parent);
    let platform_root = alias_parent.join("synthetic.app");

    // Untrusted CAS inputs must still reject links in their ancestry.
    let error = PayloadCas::new(&platform_root).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(error
        .to_string()
        .starts_with("linked path component is forbidden:"));

    let root = resolve_platform_root(&platform_root, false).unwrap();
    assert_eq!(root, resolve_platform_root(&platform_root, true).unwrap());
    assert_eq!(root, fs::canonicalize(&real_root).unwrap());
    let cas = PayloadCas::new(&root).expect("open CAS through resolved platform root");
    let payload = cas.prepare_bytes(b"synthetic Android asset").unwrap();
    assert_eq!(
        cas.read_object(&payload.content_hash).unwrap().unwrap(),
        b"synthetic Android asset"
    );
    assert!(matches!(
        cas.unlink_exact_object(
            &payload.content_hash,
            payload.byte_size,
            &payload.physical_key
        )
        .unwrap(),
        ExactObjectUnlink::Removed { .. }
    ));
    assert!(cas.read_object(&payload.content_hash).unwrap().is_none());
}

#[test]
fn platform_root_does_not_assume_primary_user_or_package_name() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("user/10/another.synthetic.app");
    fs::create_dir_all(&root).unwrap();
    assert_eq!(
        resolve_platform_root(&root, false).unwrap(),
        fs::canonicalize(root).unwrap()
    );
}

#[test]
fn platform_root_itself_cannot_be_a_link() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = fixture.path().join("linked-app");
    link_directory(outside.path(), &root);
    assert_eq!(
        resolve_platform_root(&root, false).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
}

#[test]
fn platform_root_must_be_an_existing_absolute_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let file = fixture.path().join("file");
    fs::write(&file, b"synthetic").unwrap();
    assert_eq!(
        resolve_platform_root(&file, false).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        resolve_platform_root(&fixture.path().join("missing"), false)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        resolve_platform_root(Path::new("relative-app"), false)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn first_run_creates_a_real_root_beneath_a_trusted_parent_alias() {
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path().join("real-parent");
    fs::create_dir(&parent).unwrap();
    let alias = fixture.path().join("home-alias");
    link_directory(&parent, &alias);
    let root = resolve_platform_root(&alias.join("RisuNest"), true).unwrap();
    assert_eq!(root, parent.join("RisuNest").canonicalize().unwrap());
    let cas = PayloadCas::new(&root).unwrap();
    let payload = cas.prepare_bytes(b"first-run asset").unwrap();
    assert_eq!(
        cas.read_object(&payload.content_hash).unwrap().unwrap(),
        b"first-run asset"
    );
}

#[test]
fn android_first_run_creates_only_the_owned_files_root() {
    let sandbox = tempfile::tempdir().unwrap();
    for sibling in ["shared_prefs", "app_webview", "no_backup"] {
        fs::create_dir(sandbox.path().join(sibling)).unwrap();
        fs::write(sandbox.path().join(sibling).join("synthetic"), b"keep").unwrap();
    }
    let files = sandbox.path().join("files");
    assert!(!files.exists());
    let resolved = platform_store_root(&files, Platform::Android).unwrap();
    assert_eq!(resolved, files.canonicalize().unwrap());
    assert_eq!(platform_store_root(&files, Platform::Android).unwrap(), resolved);
    for sibling in ["shared_prefs", "app_webview", "no_backup"] {
        assert_eq!(fs::read(sandbox.path().join(sibling).join("synthetic")).unwrap(), b"keep");
    }
}

#[test]
fn android_store_root_rejects_an_existing_link_or_file() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let files = sandbox.path().join("files");
    link_directory(outside.path(), &files);
    assert!(platform_store_root(&files, Platform::Android).is_err());
    let occupied = sandbox.path().join("occupied");
    fs::write(&occupied, b"synthetic").unwrap();
    assert!(platform_store_root(&occupied, Platform::Android).is_err());
}

#[test]
fn first_run_does_not_accept_a_linked_or_nonabsolute_root() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = fixture.path().join("linked-app");
    link_directory(outside.path(), &root);
    assert_eq!(
        resolve_platform_root(&root, true).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        resolve_platform_root(Path::new("relative-app"), true)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn resolved_root_does_not_allow_asset_directory_redirection() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = resolve_platform_root(fixture.path(), false).unwrap();
    let cas = PayloadCas::new(&root).unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, b"untouched").unwrap();
    link_directory(outside.path(), &root.join("assets"));
    assert_eq!(
        cas.prepare_bytes(b"new asset").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        cas.read_object(&"0".repeat(64)).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(fs::read(sentinel).unwrap(), b"untouched");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
}

#[test]
fn resolved_root_replaced_by_link_is_rejected_by_cas() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let platform_root = fixture.path().join("app");
    fs::create_dir(&platform_root).unwrap();
    let root = resolve_platform_root(&platform_root, false).unwrap();
    let cas = PayloadCas::new(&root).unwrap();
    fs::remove_dir(&platform_root).unwrap();
    link_directory(outside.path(), &platform_root);
    assert_eq!(
        cas.prepare_bytes(b"new asset").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert!(PayloadCas::new(&root).is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

fn settled(entry: &Path, create: bool) -> Result<AppPaths, super::DataRootError> {
    AppPaths::isolated(entry.to_owned()).unwrap().settled(Platform::current(), create)
}

/// Every release before links were followed used this as the data root.
fn released_root(entry: &Path) -> PathBuf {
    if cfg!(target_os = "linux") {
        fs::canonicalize(entry.parent().unwrap()).unwrap().join(entry.file_name().unwrap())
    } else {
        entry.to_owned()
    }
}

#[test]
fn a_desktop_data_root_link_leads_to_its_target_and_keeps_its_entry() {
    let fixture = tempfile::tempdir().unwrap();
    let target = fixture.path().join("moved");
    fs::create_dir(&target).unwrap();
    let entry = fixture.path().join("RisuNestData");
    link_directory(&target, &entry);
    let paths = settled(&entry, true).unwrap();
    assert_eq!(paths.data_entry, released_root(&entry));
    assert_eq!(fs::canonicalize(&paths.data).unwrap(), fs::canonicalize(&target).unwrap());
    assert!(!paths.data.to_string_lossy().starts_with(r"\\?\"));
    assert_eq!(paths.logs, paths.data.join("logs"));
    // The CAS rejects every linked component, and still works there.
    let cas = PayloadCas::new(&paths.data).unwrap();
    let payload = cas.prepare_bytes(b"synthetic moved asset").unwrap();
    assert_eq!(cas.read_object(&payload.content_hash).unwrap().unwrap(), b"synthetic moved asset");
    assert!(target.join("assets").is_dir());
    assert!(matches!(
        cas.unlink_exact_object(&payload.content_hash, payload.byte_size, &payload.physical_key).unwrap(),
        ExactObjectUnlink::Removed { .. }
    ));
    assert!(cas.read_object(&payload.content_hash).unwrap().is_none());
    assert_eq!(settled(&entry, false).unwrap(), paths);
}

#[test]
fn a_desktop_data_root_without_a_link_keeps_the_path_every_release_used() {
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path().join("real-parent");
    fs::create_dir(&parent).unwrap();
    let alias = fixture.path().join("home-alias");
    link_directory(&parent, &alias);
    for entry in [fixture.path().join("RisuNestData"), alias.join("RisuNestData")] {
        let first = settled(&entry, true).unwrap();
        assert_eq!(first.data_entry, released_root(&entry));
        assert_eq!(first.data, first.data_entry);
        // Only Linux has created it at startup.
        assert_eq!(entry.exists(), cfg!(target_os = "linux"));
        fs::create_dir_all(&entry).unwrap();
        let existing = settled(&entry, true).unwrap();
        assert_eq!((existing.data, existing.data_entry), (first.data.clone(), first.data));
    }
}

#[test]
fn a_data_root_link_whose_target_is_gone_stops_startup_and_creates_nothing() {
    let fixture = tempfile::tempdir().unwrap();
    let target = fixture.path().join("moved");
    fs::create_dir(&target).unwrap();
    let entry = fixture.path().join("RisuNestData");
    link_directory(&target, &entry);
    fs::remove_dir(&target).unwrap();
    let error = settled(&entry, true).unwrap_err();
    assert_eq!(error.refusal, Some(LinkRefusal::TargetMissing));
    assert!(error.to_string().contains("link target is missing"));
    assert!(!target.exists());
    let cleanup = settled(&entry, false).unwrap();
    assert_eq!(cleanup.data, cleanup.data_entry);
    // A link to a file is refused the same way.
    #[cfg(unix)]
    {
        fs::write(&target, b"user").unwrap();
        assert_eq!(settled(&entry, true).unwrap_err().refusal, Some(LinkRefusal::NotDirectory));
        let cleanup = settled(&entry, false).unwrap();
        assert_eq!(cleanup.data, cleanup.data_entry);
        assert_eq!(fs::read(&target).unwrap(), b"user");
    }
}

#[cfg(windows)]
#[test]
fn a_data_root_link_to_a_network_share_is_refused() {
    for path in [r"\\?\UNC\server\share\RisuNest", r"\\server\share\RisuNest"] {
        assert_eq!(super::local_path(PathBuf::from(path)).unwrap(), Err(LinkRefusal::NetworkTarget));
    }
}

#[test]
fn network_file_systems_are_told_from_local_ones() {
    // NFS, SMB, CIFS, SMB2, both AFS, Coda, NCP, Ceph.
    for magic in [0x6969, 0x517B, 0xFF53_4D42, 0xFE53_4D42, 0x5346_414F, 0x6B41_4653, 0x7375_7245, 0x564C, 0x00C3_6400] {
        assert!(super::network_file_system(magic), "{magic:#x}");
    }
    // ext4, btrfs, XFS, tmpfs, FUSE (ntfs-3g) and 9p (WSL drives) stay usable.
    for magic in [0xEF53, 0x9123_683E, 0x5846_5342, 0x0102_1994, 0x6573_5546, 0x0102_1997] {
        assert!(!super::network_file_system(magic), "{magic:#x}");
    }
    assert!(super::network_mount_flags(0));
    assert!(!super::network_mount_flags(0x1000));
    #[cfg(target_os = "macos")]
    assert!(!super::network_mount_flags(libc::MNT_LOCAL as u32));
}

#[test]
fn only_names_the_app_creates_count_as_data_root_entries() {
    for name in ["persistent", "assets", "data-root.lock", ".window-state.json", "external-connections.sqlite-wal",
        "external-jobs.sqlite-shm", ".tmpA1b2C3", "asset-hydration-x1Y2z3", "remotes", "oauthData.json"] {
        assert!(is_data_root_entry(name.as_ref()), "{name}");
    }
    for name in ["notes.txt", ".tmp", "persistent-old", "Persistent", "notes.sqlite-wal", "persistent-wal", "asset-hydration-"] {
        assert!(!is_data_root_entry(name.as_ref()), "{name}");
    }
}

#[test]
fn stored_external_storage_paths_follow_the_data_root() {
    let root = base("moved").join("RisuNestData");
    let old = base("local").join("RisuNestData").join("external-storage").join("captures").join("a").join("capture.sqlite");
    let current = root.join("external-storage").join("captures").join("a").join("capture.sqlite");
    assert_eq!(relocated_external_storage_path(&old, &root), current);
    assert_eq!(relocated_external_storage_path(&current, &root), current);
    #[cfg(windows)]
    assert_eq!(
        relocated_external_storage_path(Path::new(r"\\?\C:\old\RisuNestData\external-storage\captures\a\capture.sqlite"), &root),
        current,
    );
    // A path with no such part, or one that climbs out of it, is left for the
    // caller's containment check.
    for stored in [base("elsewhere").join("capture.sqlite"), base("old").join("external-storage").join("..").join("escape")] {
        assert_eq!(relocated_external_storage_path(&stored, &root), stored);
    }
}
