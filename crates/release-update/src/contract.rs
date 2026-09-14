use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use semver::Version;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::{Error, Result};

const CATALOG_SCHEMA: &str = "risunest.release-catalog/v1";
const PRODUCT_SCHEMA: &str = "risunest.product-release/v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Product {
    App,
    Sync,
}

impl fmt::Display for Product {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::App => "app",
            Self::Sync => "sync",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Desktop,
    Mobile,
    Raw,
    Managed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OperatingSystem {
    Windows,
    Linux,
    Darwin,
    Android,
    Ios,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Architecture {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

impl fmt::Display for Architecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum PackageFormat {
    #[serde(rename = "zip")]
    Zip,
    #[serde(rename = "nsis")]
    Nsis,
    #[serde(rename = "deb")]
    Deb,
    #[serde(rename = "appimage")]
    AppImage,
    #[serde(rename = "dmg")]
    Dmg,
    #[serde(rename = "apk")]
    Apk,
    #[serde(rename = "ipa")]
    Ipa,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "app.tar.gz")]
    AppTarGz,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseCatalog {
    pub schema: String,
    pub published_at: String,
    pub publication_tag: String,
    pub products: CatalogProducts,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogProducts {
    pub app: Option<ProductEntry>,
    pub sync: Option<ProductEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductEntry {
    pub manifest_url: String,
    pub manifest_sha256: String,
    pub release: ProductRelease,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductRelease {
    pub schema: String,
    pub product: Product,
    pub version: String,
    pub tag: String,
    pub source_commit: String,
    pub release_page: String,
    pub notes: String,
    pub localized_notes: BTreeMap<String, String>,
    #[serde(rename = "pub_date")]
    pub pub_date: String,
    pub platforms: BTreeMap<String, PlatformEntry>,
    pub downloads: Vec<Download>,
    pub compatibility: Option<Compatibility>,
    pub vendor: Vec<VendorArtifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogProductsWire {
    app: serde_json::Value,
    sync: serde_json::Value,
}

impl<'de> Deserialize<'de> for CatalogProducts {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = CatalogProductsWire::deserialize(deserializer)?;
        Ok(Self {
            app: serde_json::from_value(wire.app).map_err(serde::de::Error::custom)?,
            sync: serde_json::from_value(wire.sync).map_err(serde::de::Error::custom)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProductReleaseWire {
    schema: String,
    product: Product,
    version: String,
    tag: String,
    source_commit: String,
    release_page: String,
    notes: String,
    localized_notes: BTreeMap<String, String>,
    #[serde(rename = "pub_date")]
    pub_date: String,
    platforms: BTreeMap<String, PlatformEntry>,
    downloads: Vec<Download>,
    compatibility: serde_json::Value,
    vendor: Vec<VendorArtifact>,
}

impl<'de> Deserialize<'de> for ProductRelease {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ProductReleaseWire::deserialize(deserializer)?;
        Ok(Self {
            schema: wire.schema,
            product: wire.product,
            version: wire.version,
            tag: wire.tag,
            source_commit: wire.source_commit,
            release_page: wire.release_page,
            notes: wire.notes,
            localized_notes: wire.localized_notes,
            pub_date: wire.pub_date,
            platforms: wire.platforms,
            downloads: wire.downloads,
            compatibility: serde_json::from_value(wire.compatibility)
                .map_err(serde::de::Error::custom)?,
            vendor: wire.vendor,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformEntry {
    pub url: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Download {
    pub product: Product,
    pub variant: Variant,
    pub os: OperatingSystem,
    pub arch: Architecture,
    pub format: PackageFormat,
    pub version: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub signature_url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Compatibility {
    pub protocol_id: String,
    pub store_format_id: String,
    pub automatic_apply: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VendorArtifact {
    pub name: String,
    pub version: String,
    pub os: OperatingSystem,
    pub arch: Architecture,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackageRequest {
    pub product: Product,
    pub variant: Variant,
    pub os: OperatingSystem,
    pub arch: Architecture,
    pub format: PackageFormat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    owner: String,
    name: String,
}

impl Repository {
    pub fn risunest() -> Self {
        Self {
            owner: "rsyumi".to_owned(),
            name: "RisuNest".to_owned(),
        }
    }

    pub fn latest_catalog_url(&self) -> Url {
        Url::parse(&format!(
            "https://github.com/{}/{}/releases/latest/download/manifest.json",
            self.owner, self.name
        ))
        .expect("fixed GitHub URL is valid")
    }

    pub fn release_asset_url(&self, tag: &str, asset: &str) -> Result<Url> {
        validate_tag(tag)?;
        if asset.is_empty()
            || asset.len() > 240
            || !asset.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
            })
        {
            return Err(Error::DisallowedUrl(asset.to_owned()));
        }
        Url::parse(&format!(
            "https://github.com/{}/{}/releases/download/{tag}/{asset}",
            self.owner, self.name
        ))
        .map_err(|error| Error::DisallowedUrl(error.to_string()))
    }

    pub fn validate_asset_url(&self, value: &str, tag: &str) -> Result<Url> {
        validate_tag(tag)?;
        let url = Url::parse(value).map_err(|error| Error::DisallowedUrl(error.to_string()))?;
        if url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::DisallowedUrl(value.to_owned()));
        }
        let segments = url
            .path_segments()
            .map(|segments| segments.collect::<Vec<_>>())
            .unwrap_or_default();
        if segments.len() != 6
            || segments[0] != self.owner
            || segments[1] != self.name
            || segments[2] != "releases"
            || segments[3] != "download"
            || segments[4] != tag
            || segments[5].is_empty()
            || segments[5].len() > 240
            || !segments[5].bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
            })
        {
            return Err(Error::DisallowedUrl(value.to_owned()));
        }
        Ok(url)
    }

    pub fn validate_release_page(&self, value: &str, tag: &str) -> Result<()> {
        let url = Url::parse(value).map_err(|error| Error::DisallowedUrl(error.to_string()))?;
        let expected = format!(
            "https://github.com/{}/{}/releases/tag/{tag}",
            self.owner, self.name
        );
        if url.as_str().trim_end_matches('/') != expected {
            return Err(Error::DisallowedUrl(value.to_owned()));
        }
        Ok(())
    }
}

impl ReleaseCatalog {
    pub fn parse(bytes: &[u8], repository: &Repository) -> Result<Self> {
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|error| Error::Decode(error.to_string()))?;
        catalog.validate(repository)?;
        Ok(catalog)
    }

    pub fn validate(&self, repository: &Repository) -> Result<()> {
        if self.schema != CATALOG_SCHEMA {
            return invalid("catalog schema");
        }
        parse_time(&self.published_at, "publishedAt")?;
        validate_tag(&self.publication_tag)?;
        if self.products.app.is_none() && self.products.sync.is_none() {
            return invalid("catalog must contain a product");
        }
        for (product, entry) in [
            (Product::App, &self.products.app),
            (Product::Sync, &self.products.sync),
        ] {
            let Some(entry) = entry else { continue };
            entry.validate(repository)?;
            if entry.release.product != product {
                return invalid("catalog product slot mismatch");
            }
            if !entry.release.semver()?.pre.is_empty() {
                return invalid("stable catalog contains prerelease");
            }
        }
        let publication_product = if self.publication_tag.starts_with("app-v") {
            Product::App
        } else {
            Product::Sync
        };
        if self.product(publication_product)?.release.tag != self.publication_tag {
            return invalid("catalog publicationTag mismatch");
        }
        Ok(())
    }

    pub fn product(&self, product: Product) -> Result<&ProductEntry> {
        let entry = match product {
            Product::App => self.products.app.as_ref(),
            Product::Sync => self.products.sync.as_ref(),
        };
        entry.ok_or(Error::ProductUnavailable(product))
    }
}

impl ProductEntry {
    pub fn validate(&self, repository: &Repository) -> Result<()> {
        self.release.validate(repository)?;
        validate_sha256(&self.manifest_sha256, "manifestSha256")?;
        let url = repository.validate_asset_url(&self.manifest_url, &self.release.tag)?;
        if url
            .path_segments()
            .and_then(|mut values| values.next_back())
            != Some("product-manifest.json")
        {
            return invalid("product manifest asset name");
        }
        Ok(())
    }
}

impl ProductRelease {
    pub fn parse(bytes: &[u8], repository: &Repository) -> Result<Self> {
        let release: Self =
            serde_json::from_slice(bytes).map_err(|error| Error::Decode(error.to_string()))?;
        release.validate(repository)?;
        Ok(release)
    }

    pub fn semver(&self) -> Result<Version> {
        parse_version(&self.version)
    }

    pub fn validate(&self, repository: &Repository) -> Result<()> {
        if self.schema != PRODUCT_SCHEMA {
            return invalid("product schema");
        }
        let version = self.semver()?;
        if !version.build.is_empty() {
            return invalid("release version must not contain build metadata");
        }
        let expected_tag = format!("{}-v{}", self.product, self.version);
        if self.tag != expected_tag {
            return invalid("product tag/version mismatch");
        }
        if self.source_commit.len() != 40
            || !self
                .source_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return invalid("sourceCommit");
        }
        repository.validate_release_page(&self.release_page, &self.tag)?;
        parse_time(&self.pub_date, "pub_date")?;
        if self.notes.as_bytes().len() > 4096
            || self
                .localized_notes
                .iter()
                .any(|(locale, notes)| !valid_locale(locale) || notes.as_bytes().len() > 4096)
        {
            return invalid("release notes");
        }
        match (self.product, self.compatibility.as_ref()) {
            (Product::App, None) => {}
            (Product::Sync, Some(value))
                if !value.protocol_id.is_empty() && !value.store_format_id.is_empty() => {}
            (Product::App, Some(_)) => return invalid("app compatibility must be null"),
            (Product::Sync, _) => return invalid("sync compatibility is required"),
        }
        if self.product == Product::App && !self.vendor.is_empty() {
            return invalid("app vendor must be empty");
        }
        if self.product == Product::Sync && self.vendor.is_empty() {
            return invalid("sync vendor is required");
        }
        for vendor in &self.vendor {
            if vendor.name != "cloudflared"
                || vendor.version.is_empty()
                || !matches!(
                    vendor.os,
                    OperatingSystem::Windows | OperatingSystem::Linux | OperatingSystem::Darwin
                )
            {
                return invalid("vendor identity");
            }
            validate_sha256(&vendor.sha256, "vendor sha256")?;
        }

        let mut identities = BTreeSet::new();
        let mut urls = BTreeSet::new();
        for download in &self.downloads {
            download.validate(repository, self)?;
            if !identities.insert((
                download.product,
                download.variant,
                download.os,
                download.arch,
                download.format,
            )) {
                return invalid("duplicate download identity");
            }
            if !urls.insert(download.url.as_str()) {
                return invalid("duplicate download URL");
            }
        }
        let required = required_downloads(self.product);
        if identities != required {
            return invalid("required download matrix");
        }
        let expected_platforms = expected_platforms(self.product);
        if self.platforms.keys().cloned().collect::<BTreeSet<_>>()
            != expected_platforms.keys().cloned().collect()
        {
            return invalid("platform target matrix");
        }
        for (target, request) in expected_platforms {
            let platform = &self.platforms[&target];
            if platform.signature.trim().is_empty() {
                return invalid("platform signature");
            }
            repository.validate_asset_url(&platform.url, &self.tag)?;
            let download = self.select_download(&request).ok();
            if download.map(|download| download.url.as_str()) != Some(platform.url.as_str()) {
                return invalid("platform/download mismatch");
            }
        }
        Ok(())
    }

    pub fn select_download(&self, request: &PackageRequest) -> Result<&Download> {
        self.downloads
            .iter()
            .find(|download| {
                download.product == request.product
                    && download.variant == request.variant
                    && download.os == request.os
                    && download.arch == request.arch
                    && download.format == request.format
            })
            .ok_or(Error::PackageUnavailable)
    }

    pub fn platform(&self, target: &str) -> Result<&PlatformEntry> {
        self.platforms.get(target).ok_or(Error::PackageUnavailable)
    }
}

impl Download {
    fn validate(&self, repository: &Repository, release: &ProductRelease) -> Result<()> {
        if self.product != release.product || self.version != release.version || self.size == 0 {
            return invalid("download product/version/size");
        }
        match (self.product, self.variant) {
            (Product::App, Variant::Desktop | Variant::Mobile)
            | (Product::Sync, Variant::Raw | Variant::Managed) => {}
            _ => return invalid("download variant"),
        }
        validate_sha256(&self.sha256, "download sha256")?;
        repository.validate_asset_url(&self.url, &release.tag)?;
        repository.validate_asset_url(&self.signature_url, &release.tag)?;
        if self.signature_url != format!("{}.sig", self.url) {
            return invalid("download signature URL");
        }
        Ok(())
    }
}

pub fn compare_versions(left: &str, right: &str) -> Result<std::cmp::Ordering> {
    Ok(parse_version(left)?.cmp(&parse_version(right)?))
}

fn parse_version(value: &str) -> Result<Version> {
    Version::parse(value).map_err(|error| Error::InvalidMetadata(format!("version: {error}")))
}

fn validate_tag(tag: &str) -> Result<()> {
    let Some(version) = tag
        .strip_prefix("app-v")
        .or_else(|| tag.strip_prefix("sync-v"))
    else {
        return invalid("release tag");
    };
    let parsed = parse_version(version)?;
    if !parsed.build.is_empty() || tag.contains('/') {
        return invalid("release tag");
    }
    Ok(())
}

fn validate_sha256(value: &str, field: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return invalid(field);
    }
    Ok(())
}

fn parse_time(value: &str, field: &str) -> Result<()> {
    time::OffsetDateTime::parse(value, &Rfc3339)
        .map(|_| ())
        .map_err(|error| Error::InvalidMetadata(format!("{field}: {error}")))
}

fn valid_locale(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(language) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&language.len()) || !language.bytes().all(|byte| byte.is_ascii_lowercase())
    {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => true,
        (Some(region), None) => {
            (2..=8).contains(&region.len()) && region.bytes().all(|byte| byte.is_ascii_alphabetic())
        }
        _ => false,
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidMetadata(message.into()))
}

fn required_downloads(
    product: Product,
) -> BTreeSet<(
    Product,
    Variant,
    OperatingSystem,
    Architecture,
    PackageFormat,
)> {
    use Architecture::{Aarch64, X86_64};
    use OperatingSystem::{Android, Darwin, Ios, Linux, Windows};
    use PackageFormat::{Apk, AppImage, AppTarGz, Deb, Dmg, Ipa, Nsis, TarGz, Zip};
    use Variant::{Desktop, Managed, Mobile, Raw};
    let mut result = BTreeSet::new();
    for arch in [X86_64, Aarch64] {
        match product {
            Product::App => {
                for (os, format) in [
                    (Windows, Zip),
                    (Windows, Nsis),
                    (Linux, Deb),
                    (Linux, AppImage),
                    (Darwin, Dmg),
                    (Darwin, AppTarGz),
                ] {
                    result.insert((product, Desktop, os, arch, format));
                }
            }
            Product::Sync => {
                for (variant, os, format) in [
                    (Raw, Windows, Zip),
                    (Raw, Linux, TarGz),
                    (Raw, Darwin, TarGz),
                    (Managed, Windows, Zip),
                    (Managed, Windows, Nsis),
                    (Managed, Linux, TarGz),
                    (Managed, Darwin, Dmg),
                    (Managed, Darwin, AppTarGz),
                ] {
                    result.insert((product, variant, os, arch, format));
                }
            }
        }
    }
    if product == Product::App {
        result.insert((product, Mobile, Android, Aarch64, Apk));
        result.insert((product, Mobile, Ios, Aarch64, Ipa));
    }
    result
}

fn expected_platforms(product: Product) -> BTreeMap<String, PackageRequest> {
    let mut result = BTreeMap::new();
    for arch in [Architecture::X86_64, Architecture::Aarch64] {
        result.insert(
            format!("windows-{arch}-nsis"),
            PackageRequest {
                product,
                variant: Variant::Managed,
                os: OperatingSystem::Windows,
                arch,
                format: PackageFormat::Nsis,
            },
        );
        if product == Product::App {
            result
                .get_mut(&format!("windows-{arch}-nsis"))
                .unwrap()
                .variant = Variant::Desktop;
            result.insert(
                format!("linux-{arch}-deb"),
                PackageRequest {
                    product,
                    variant: Variant::Desktop,
                    os: OperatingSystem::Linux,
                    arch,
                    format: PackageFormat::Deb,
                },
            );
            result.insert(
                format!("linux-{arch}-appimage"),
                PackageRequest {
                    product,
                    variant: Variant::Desktop,
                    os: OperatingSystem::Linux,
                    arch,
                    format: PackageFormat::AppImage,
                },
            );
        }
        result.insert(
            format!("darwin-{arch}-app"),
            PackageRequest {
                product,
                variant: if product == Product::App {
                    Variant::Desktop
                } else {
                    Variant::Managed
                },
                os: OperatingSystem::Darwin,
                arch,
                format: PackageFormat::AppTarGz,
            },
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_rejects_latest_assets_and_other_repositories() {
        let repository = Repository::risunest();
        assert!(repository
            .validate_asset_url(
                "https://github.com/rsyumi/RisuNest/releases/download/app-v1.2.3/app.zip",
                "app-v1.2.3"
            )
            .is_ok());
        for value in [
            "http://github.com/rsyumi/RisuNest/releases/download/app-v1.2.3/app.zip",
            "https://github.com/other/RisuNest/releases/download/app-v1.2.3/app.zip",
            "https://github.com/rsyumi/RisuNest/releases/latest/download/app.zip",
            "https://objects.githubusercontent.com/file",
        ] {
            assert!(
                repository.validate_asset_url(value, "app-v1.2.3").is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn semantic_versions_are_compared_numerically() {
        assert_eq!(
            compare_versions("2026.10.1", "2026.9.99").unwrap(),
            std::cmp::Ordering::Greater
        );
        assert!(compare_versions("2026.09.1", "2026.9.1").is_err());
    }
}
