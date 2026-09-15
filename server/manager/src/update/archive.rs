use crate::Result;
use risunest_release_update::{
    safe_archive_path, validate_archive_entry, Architecture, ArchiveEntryKind, OperatingSystem,
    PackageFormat, VendorArtifact,
};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

const INVENTORY_SCHEMA: &str = "risunest-sync-bundle/v1";
const INVENTORY_FILE: &str = "risunest-sync-bundle.json";

#[derive(Debug)]
pub struct ExtractedBundle {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Inventory {
    schema: String,
    product: String,
    variant: String,
    version: String,
    protocol_id: String,
    store_format_id: String,
    files: Vec<String>,
    vendor: Vec<InventoryVendor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InventoryVendor {
    name: String,
    version: String,
    os: OperatingSystem,
    arch: Architecture,
    sha256: String,
    path: String,
}

fn create_entry(root: &Path, name: &str, kind: ArchiveEntryKind) -> Result<PathBuf> {
    validate_archive_entry(name, kind).map_err(|_| "update-archive-entry-unsafe".to_owned())?;
    let path =
        safe_archive_path(root, name).map_err(|_| "update-archive-entry-unsafe".to_owned())?;
    if kind == ArchiveEntryKind::Directory {
        fs::create_dir(&path).map_err(|_| "update-archive-duplicate-or-invalid".to_owned())?;
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| "update-archive-extract-failed".to_owned())?;
        }
    }
    Ok(path)
}

fn tar_kind(kind: tar::EntryType) -> ArchiveEntryKind {
    if kind.is_file() {
        ArchiveEntryKind::File
    } else if kind.is_dir() {
        ArchiveEntryKind::Directory
    } else if kind.is_symlink() {
        ArchiveEntryKind::Symlink
    } else if kind.is_hard_link() {
        ArchiveEntryKind::Hardlink
    } else if kind.is_block_special() || kind.is_character_special() {
        ArchiveEntryKind::Device
    } else if kind.is_fifo() {
        ArchiveEntryKind::Fifo
    } else {
        ArchiveEntryKind::Other
    }
}

fn extract_tar_gz(archive: &Path, output: &Path) -> Result<Vec<PathBuf>> {
    let file = File::open(archive).map_err(|_| "update-package-open-failed".to_owned())?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for entry in archive
        .entries()
        .map_err(|_| "update-archive-invalid".to_owned())?
    {
        let mut entry = entry.map_err(|_| "update-archive-invalid".to_owned())?;
        let raw = entry.path_bytes();
        let name = std::str::from_utf8(&raw)
            .map_err(|_| "update-archive-entry-unsafe".to_owned())?
            .to_owned();
        if !seen.insert(name.clone()) {
            return Err("update-archive-duplicate-or-invalid".into());
        }
        let kind = tar_kind(entry.header().entry_type());
        let path = create_entry(output, &name, kind)?;
        if kind == ArchiveEntryKind::File {
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|_| "update-archive-duplicate-or-invalid".to_owned())?;
            io::copy(&mut entry, &mut target)
                .map_err(|_| "update-archive-extract-failed".to_owned())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry
                    .header()
                    .mode()
                    .map_err(|_| "update-archive-invalid".to_owned())?
                    & 0o777;
                fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                    .map_err(|_| "update-archive-extract-failed".to_owned())?;
            }
            target
                .sync_all()
                .map_err(|_| "update-archive-extract-failed".to_owned())?;
            files.push(PathBuf::from(name));
        }
    }
    Ok(files)
}

fn extract_zip(archive: &Path, output: &Path) -> Result<Vec<PathBuf>> {
    let file = File::open(archive).map_err(|_| "update-package-open-failed".to_owned())?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|_| "update-archive-invalid".to_owned())?;
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| "update-archive-invalid".to_owned())?;
        let name = entry.name().to_owned();
        if !seen.insert(name.clone()) {
            return Err("update-archive-duplicate-or-invalid".into());
        }
        let unix_type = entry.unix_mode().unwrap_or(0) & 0o170000;
        let kind = if entry.is_dir() {
            ArchiveEntryKind::Directory
        } else if unix_type == 0o120000 {
            ArchiveEntryKind::Symlink
        } else if unix_type == 0 || unix_type == 0o100000 {
            ArchiveEntryKind::File
        } else {
            ArchiveEntryKind::Other
        };
        let path = create_entry(output, &name, kind)?;
        if kind == ArchiveEntryKind::File {
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|_| "update-archive-duplicate-or-invalid".to_owned())?;
            io::copy(&mut entry, &mut target)
                .map_err(|_| "update-archive-extract-failed".to_owned())?;
            target
                .sync_all()
                .map_err(|_| "update-archive-extract-failed".to_owned())?;
            files.push(PathBuf::from(name));
        }
    }
    Ok(files)
}

fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path).map_err(|_| "update-bundle-inventory-invalid".to_owned())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "update-bundle-inventory-invalid".to_owned())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
pub(crate) fn sync_bundle_tree(path: &Path) -> Result<()> {
    if !path.is_dir() {
        return Err("update-staging-invalid".into());
    }
    for entry in fs::read_dir(path).map_err(|_| "update-staging-sync-failed".to_owned())? {
        let entry = entry.map_err(|_| "update-staging-sync-failed".to_owned())?;
        let metadata = entry
            .file_type()
            .map_err(|_| "update-staging-sync-failed".to_owned())?;
        if metadata.is_symlink() {
            return Err("update-install-path-linked".into());
        }
        if metadata.is_dir() {
            sync_bundle_tree(&entry.path())?;
        } else if metadata.is_file() {
            File::open(entry.path())
                .and_then(|file| file.sync_all())
                .map_err(|_| "update-staging-sync-failed".to_owned())?;
        } else {
            return Err("update-staging-invalid".into());
        }
    }
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "update-staging-sync-failed".to_owned())?;
    File::open(path.parent().ok_or("update-staging-invalid")?)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "update-staging-sync-failed".to_owned())
}

#[cfg(windows)]
pub(crate) fn sync_bundle_tree(_path: &Path) -> Result<()> {
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_inventory(
    bundle_root: &Path,
    all_files: &[PathBuf],
    marker_path: &Path,
    relative_prefix: &Path,
    expected_version: &str,
    expected_vendor: &[VendorArtifact],
    target_os: OperatingSystem,
    target_arch: Architecture,
) -> Result<Vec<PathBuf>> {
    let bytes = fs::read(marker_path).map_err(|_| "update-bundle-inventory-missing".to_owned())?;
    let inventory: Inventory =
        serde_json::from_slice(&bytes).map_err(|_| "update-bundle-inventory-invalid".to_owned())?;
    if inventory.schema != INVENTORY_SCHEMA
        || inventory.product != "sync"
        || inventory.variant != "managed"
        || inventory.version != expected_version
        || inventory.protocol_id != risunest_sync_server::PROTOCOL_ID
        || inventory.store_format_id != risunest_sync_server::STORE_FORMAT_ID
        || inventory.files.is_empty()
    {
        return Err("update-bundle-inventory-invalid".into());
    }
    let mut declared = BTreeSet::new();
    for name in inventory.files {
        validate_archive_entry(&name, ArchiveEntryKind::File)
            .map_err(|_| "update-bundle-inventory-invalid".to_owned())?;
        let path = PathBuf::from(name);
        if path == Path::new(INVENTORY_FILE) || !declared.insert(path) {
            return Err("update-bundle-inventory-invalid".into());
        }
    }
    let marker_relative = marker_path
        .strip_prefix(bundle_root)
        .map_err(|_| "update-bundle-inventory-invalid".to_owned())?;
    let actual = all_files
        .iter()
        .filter(|path| path.as_path() != marker_relative)
        .filter_map(|path| path.strip_prefix(relative_prefix).ok().map(Path::to_owned))
        .collect::<BTreeSet<_>>();
    if actual != declared {
        return Err("update-bundle-inventory-mismatch".into());
    }
    if inventory.vendor.len() != 1 {
        return Err("update-vendor-inventory-mismatch".into());
    }
    for item in &inventory.vendor {
        let target_matches = item.os == target_os
            && (item.arch == target_arch
                || (target_os == OperatingSystem::Windows
                    && target_arch == Architecture::Aarch64
                    && item.arch == Architecture::X86_64));
        if !target_matches
            || !expected_vendor.iter().any(|expected| {
                item.name == expected.name
                    && item.version == expected.version
                    && item.os == expected.os
                    && item.arch == expected.arch
                    && item.sha256 == expected.sha256
            })
        {
            return Err("update-vendor-inventory-mismatch".into());
        }
        validate_archive_entry(&item.path, ArchiveEntryKind::File)
            .map_err(|_| "update-vendor-inventory-mismatch".to_owned())?;
        let vendor_path = bundle_root.join(relative_prefix).join(&item.path);
        if hash_file(&vendor_path)? != item.sha256 {
            return Err("update-vendor-file-invalid".into());
        }
    }
    let mut result = declared.into_iter().collect::<Vec<_>>();
    result.push(
        marker_relative
            .strip_prefix(relative_prefix)
            .unwrap_or(marker_relative)
            .to_owned(),
    );
    Ok(result)
}

pub fn extract_package(
    format: PackageFormat,
    archive: &Path,
    output: &Path,
    expected_version: &str,
    expected_vendor: &[VendorArtifact],
    target_os: OperatingSystem,
    target_arch: Architecture,
) -> Result<ExtractedBundle> {
    if output.exists() {
        return Err("update-staging-already-exists".into());
    }
    fs::create_dir(output).map_err(|_| "update-staging-create-failed".to_owned())?;
    let files = match format {
        PackageFormat::Zip => extract_zip(archive, output),
        PackageFormat::TarGz | PackageFormat::AppTarGz => extract_tar_gz(archive, output),
        _ => Err("update-package-format-unsupported".into()),
    };
    let files = match files {
        Ok(files) => files,
        Err(error) => {
            let _ = fs::remove_dir_all(output);
            return Err(error);
        }
    };
    let bundle: Result<ExtractedBundle> = match format {
        PackageFormat::Zip => {
            for required in [
                "risunest-sync-gui.exe",
                "risunest-sync-server.exe",
                "risunest-sync-manager.exe",
                "cloudflared.exe",
                "CLOUDFLARED-LICENSE",
            ] {
                if !files.iter().any(|path| path == Path::new(required)) {
                    return Err("update-package-file-missing".into());
                }
            }
            Ok(ExtractedBundle {
                root: output.to_owned(),
                files: validate_inventory(
                    output,
                    &files,
                    &output.join(INVENTORY_FILE),
                    Path::new(""),
                    expected_version,
                    expected_vendor,
                    target_os,
                    target_arch,
                )?,
            })
        }
        PackageFormat::TarGz => {
            for required in [
                "risunest-sync-server",
                "risunest-sync-manager",
                "cloudflared",
                "CLOUDFLARED-LICENSE",
                INVENTORY_FILE,
            ] {
                if !files.iter().any(|path| path == Path::new(required)) {
                    return Err("update-package-file-missing".into());
                }
            }
            let owned = validate_inventory(
                output,
                &files,
                &output.join(INVENTORY_FILE),
                Path::new(""),
                expected_version,
                expected_vendor,
                target_os,
                target_arch,
            )?;
            Ok(ExtractedBundle {
                root: output.to_owned(),
                files: owned,
            })
        }
        PackageFormat::AppTarGz => {
            let prefix = Path::new("RisuNest Sync.app");
            if files.is_empty() || files.iter().any(|path| !path.starts_with(prefix)) {
                return Err("update-app-bundle-invalid".into());
            }
            let app = output.join(prefix);
            validate_inventory(
                output,
                &files,
                &app.join("Contents/Resources").join(INVENTORY_FILE),
                prefix,
                expected_version,
                expected_vendor,
                target_os,
                target_arch,
            )?;
            Ok(ExtractedBundle {
                root: app,
                files: Vec::new(),
            })
        }
        _ => Err("update-package-format-unsupported".into()),
    };
    let bundle = bundle?;
    sync_bundle_tree(&bundle.root)?;
    Ok(bundle)
}

pub fn validate_installed_marker(install: &Path, expected_version: &str) -> Result<Vec<PathBuf>> {
    let marker = if cfg!(target_os = "macos") {
        install.join("Contents/Resources").join(INVENTORY_FILE)
    } else {
        install.join(INVENTORY_FILE)
    };
    let bytes = fs::read(&marker).map_err(|_| "managed-install-marker-missing".to_owned())?;
    let inventory: Inventory =
        serde_json::from_slice(&bytes).map_err(|_| "managed-install-marker-invalid".to_owned())?;
    if inventory.schema != INVENTORY_SCHEMA
        || inventory.product != "sync"
        || inventory.variant != "managed"
        || inventory.version != expected_version
        || inventory.protocol_id != risunest_sync_server::PROTOCOL_ID
        || inventory.store_format_id != risunest_sync_server::STORE_FORMAT_ID
    {
        return Err("managed-install-marker-invalid".into());
    }
    let mut files = BTreeSet::new();
    for name in inventory.files {
        validate_archive_entry(&name, ArchiveEntryKind::File)
            .map_err(|_| "managed-install-marker-invalid".to_owned())?;
        let relative = PathBuf::from(name);
        if relative == Path::new(INVENTORY_FILE)
            || !files.insert(relative.clone())
            || !install.join(&relative).is_file()
        {
            return Err("managed-install-marker-invalid".into());
        }
    }
    let marker_relative = marker
        .strip_prefix(install)
        .map_err(|_| "managed-install-marker-invalid".to_owned())?
        .to_owned();
    files.insert(marker_relative);
    Ok(files.into_iter().collect())
}

pub fn managed_bundle_version(install: &Path) -> Result<String> {
    let marker = if cfg!(target_os = "macos") {
        install.join("Contents/Resources").join(INVENTORY_FILE)
    } else {
        install.join(INVENTORY_FILE)
    };
    let bytes = fs::read(marker).map_err(|_| "managed-install-marker-missing".to_owned())?;
    let inventory: Inventory =
        serde_json::from_slice(&bytes).map_err(|_| "managed-install-marker-invalid".to_owned())?;
    let version = inventory.version.clone();
    validate_installed_marker(install, &version)?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    const WINDOWS_FILES: [&str; 5] = [
        "risunest-sync-gui.exe",
        "risunest-sync-server.exe",
        "risunest-sync-manager.exe",
        "cloudflared.exe",
        "CLOUDFLARED-LICENSE",
    ];

    fn synthetic_vendor(os: OperatingSystem, arch: Architecture) -> VendorArtifact {
        VendorArtifact {
            name: "cloudflared".into(),
            version: "test".into(),
            os,
            arch,
            sha256: hex::encode(Sha256::digest(b"synthetic")),
        }
    }

    fn write_windows_bundle(path: &Path, os: OperatingSystem, arch: Architecture) {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for name in WINDOWS_FILES {
            zip.start_file(name, options).unwrap();
            zip.write_all(b"synthetic").unwrap();
        }
        let vendor = synthetic_vendor(os, arch);
        zip.start_file(INVENTORY_FILE, options).unwrap();
        zip.write_all(
            serde_json::to_string(&serde_json::json!({
                "schema": INVENTORY_SCHEMA,
                "product": "sync",
                "variant": "managed",
                "version": "2.0.0",
                "protocolId": risunest_sync_server::PROTOCOL_ID,
                "storeFormatId": risunest_sync_server::STORE_FORMAT_ID,
                "files": WINDOWS_FILES,
                "vendor": [{
                    "name": vendor.name,
                    "version": vendor.version,
                    "os": vendor.os,
                    "arch": vendor.arch,
                    "sha256": vendor.sha256,
                    "path": "cloudflared.exe"
                }]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn zip_inventory_cannot_hide_or_add_files() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("bundle.zip");
        write_windows_bundle(
            &archive_path,
            OperatingSystem::Windows,
            Architecture::X86_64,
        );
        let expected = synthetic_vendor(OperatingSystem::Windows, Architecture::X86_64);
        let unrelated = synthetic_vendor(OperatingSystem::Linux, Architecture::X86_64);
        let bundle = extract_package(
            PackageFormat::Zip,
            &archive_path,
            &temp.path().join("stage"),
            "2.0.0",
            &[unrelated, expected],
            OperatingSystem::Windows,
            Architecture::X86_64,
        )
        .unwrap();
        assert!(bundle
            .files
            .contains(&PathBuf::from("risunest-sync-manager.exe")));
        assert_eq!(
            fs::read_to_string(bundle.root.join("cloudflared.exe")).unwrap(),
            "synthetic"
        );
    }

    #[test]
    fn zip_rejects_parent_paths_before_writing_outside_staging() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("bad.zip");
        let file = File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("../escape", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"bad").unwrap();
        zip.finish().unwrap();
        assert_eq!(
            extract_package(
                PackageFormat::Zip,
                &archive_path,
                &temp.path().join("stage"),
                "2.0.0",
                &[],
                OperatingSystem::Windows,
                Architecture::X86_64,
            )
            .unwrap_err(),
            "update-archive-entry-unsafe"
        );
        assert!(!temp.path().join("escape").exists());
    }

    #[test]
    #[ignore = "requires a package.mjs managed ZIP fixture"]
    fn package_script_windows_zip_matches_the_runtime_extractor() {
        let archive = PathBuf::from(std::env::var_os("RISUNEST_PACKAGE_FIXTURE").unwrap());
        let format = match std::env::var("RISUNEST_PACKAGE_FORMAT").as_deref() {
            Ok("tar.gz") => PackageFormat::TarGz,
            Ok("app.tar.gz") => PackageFormat::AppTarGz,
            _ => PackageFormat::Zip,
        };
        let target_os = match std::env::var("RISUNEST_PACKAGE_OS").as_deref() {
            Ok("linux") => OperatingSystem::Linux,
            Ok("darwin") => OperatingSystem::Darwin,
            _ => OperatingSystem::Windows,
        };
        let target_arch = match std::env::var("RISUNEST_PACKAGE_ARCH").as_deref() {
            Ok("aarch64") => Architecture::Aarch64,
            _ => Architecture::X86_64,
        };
        let (version, vendor) = if let Some(path) = std::env::var_os("RISUNEST_PRODUCT_MANIFEST") {
            let release: risunest_release_update::ProductRelease =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            (release.version, release.vendor)
        } else {
            (
                std::env::var("RISUNEST_PACKAGE_VERSION").unwrap(),
                vec![VendorArtifact {
                    name: "cloudflared".into(),
                    version: std::env::var("RISUNEST_PACKAGE_VENDOR_VERSION").unwrap(),
                    os: OperatingSystem::Windows,
                    arch: Architecture::X86_64,
                    sha256: std::env::var("RISUNEST_PACKAGE_VENDOR_SHA256").unwrap(),
                }],
            )
        };
        let temp = tempfile::tempdir().unwrap();
        let bundle = extract_package(
            format,
            &archive,
            &temp.path().join("extracted"),
            &version,
            &vendor,
            target_os,
            target_arch,
        )
        .unwrap();
        if format == PackageFormat::Zip {
            for required in [
                "risunest-sync-gui.exe",
                "risunest-sync-server.exe",
                "risunest-sync-manager.exe",
                "cloudflared.exe",
                "CLOUDFLARED-LICENSE",
                INVENTORY_FILE,
            ] {
                assert!(bundle.files.contains(&PathBuf::from(required)));
            }
        } else {
            assert!(bundle.root.exists());
        }
    }

    #[test]
    fn vendor_proof_must_match_the_selected_target_os() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("wrong-os.zip");
        write_windows_bundle(&archive, OperatingSystem::Linux, Architecture::X86_64);
        let signed = synthetic_vendor(OperatingSystem::Linux, Architecture::X86_64);
        assert_eq!(
            extract_package(
                PackageFormat::Zip,
                &archive,
                &temp.path().join("stage"),
                "2.0.0",
                &[signed],
                OperatingSystem::Windows,
                Architecture::X86_64,
            )
            .unwrap_err(),
            "update-vendor-inventory-mismatch"
        );
    }

    #[test]
    fn vendor_proof_must_match_the_selected_target_architecture() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("wrong-arch.zip");
        write_windows_bundle(&archive, OperatingSystem::Windows, Architecture::Aarch64);
        let signed = synthetic_vendor(OperatingSystem::Windows, Architecture::Aarch64);
        assert_eq!(
            extract_package(
                PackageFormat::Zip,
                &archive,
                &temp.path().join("stage"),
                "2.0.0",
                &[signed],
                OperatingSystem::Windows,
                Architecture::X86_64,
            )
            .unwrap_err(),
            "update-vendor-inventory-mismatch"
        );
    }

    #[test]
    fn windows_arm64_accepts_the_pinned_x86_64_vendor_binary() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("arm64.zip");
        write_windows_bundle(&archive, OperatingSystem::Windows, Architecture::X86_64);
        let signed = synthetic_vendor(OperatingSystem::Windows, Architecture::X86_64);
        extract_package(
            PackageFormat::Zip,
            &archive,
            &temp.path().join("stage"),
            "2.0.0",
            &[signed],
            OperatingSystem::Windows,
            Architecture::Aarch64,
        )
        .unwrap();
    }
}
