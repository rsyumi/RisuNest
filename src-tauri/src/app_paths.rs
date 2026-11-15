//! The single source of every filesystem root this application owns.
//!
//! No other module may call `dirs::*`, a `tauri::Manager::path()` accessor, or
//! read the environment for a root. Callers take the resolved manifest from
//! managed state. `tests/pathBoundary.node-test.mjs` enforces that boundary.

use crate::launch_profile::ProfileName;
use std::path::{Path, PathBuf};

// Directory leaf names. A platform naming decision belongs here and nowhere
// else, so that it can never be expressed through the bundle identifier again.
const WINDOWS_DATA_LEAF: &str = "RisuNestData";
const WINDOWS_WEBVIEW_LEAF: &str = "RisuNestWebViewData";
const WINDOWS_CACHE_LEAF: &str = "RisuNestCache";
const WINDOWS_CLEANUP_LEAF: &str = "RisuNest-cleanup";
const WINDOWS_INSTALL_LEAF: &str = "RisuNest";
const MACOS_DATA_LEAF: &str = "RisuNest";
const MACOS_CACHE_LEAF: &str = "RisuNest";
const MACOS_CLEANUP_LEAF: &str = "RisuNest-cleanup";
const MACOS_INSTALL_DIR: &str = "/Applications/RisuNest.app";
const LINUX_DATA_LEAF: &str = "risunest";
const LINUX_WEBVIEW_LEAF: &str = "risunest-webview";
const LINUX_CACHE_LEAF: &str = "risunest";
const LINUX_CLEANUP_LEAF: &str = "risunest-cleanup";
const LINUX_INSTALL_DIRS: [&str; 2] = ["/usr/bin", "/usr/lib/risunest"];
const MOBILE_CLEANUP_LEAF: &str = "risunest-cleanup";
const WINDOWS_PROFILES_LEAF: &str = "RisuNestProfiles";
const MACOS_PROFILES_LEAF: &str = "RisuNestProfiles";
const LINUX_PROFILES_LEAF: &str = "risunest-profiles";
const PROFILE_DATA_LEAF: &str = "data";
const PROFILE_WEBVIEW_LEAF: &str = "webview";
const PROFILE_CACHE_LEAF: &str = "cache";
const PROFILE_CLEANUP_LEAF: &str = "cleanup";
const LOGS_LEAF: &str = "logs";
/// An agent build carries the installed product name, so every owned root takes
/// a distinct leaf and an agent run cannot open real user data.
const AGENT_SUFFIX: &str = "-agent";
const CLEANUP_SUFFIX: &str = "-cleanup";
/// Held inside the data root by the instance using it.
pub(crate) const DATA_ROOT_LOCK: &str = "data-root.lock";
/// The names a desktop instance creates directly inside the data root, native
/// and renderer alike, and the OS metadata a file manager adds. Cleanup of a
/// linked data root deletes only these, so a name missing here leaves that
/// entry behind and never deletes anything else.
const DATA_ROOT_ENTRIES: &[&str] = &[
    DATA_ROOT_LOCK, LOGS_LEAF, "persistent", "assets", "network-logs", "native-file-jobs", "native-media-ipc",
    "external-storage", "device-backup", "boot", "server-sync", "external-sync", "descriptor-staging",
    "recovery-staging", "source-preservation", "owned-secret-index", "external-storage-secrets",
    "external-storage-root-keys", "account-credentials", "server-sync-credentials", "device-secret-files",
    "external-jobs.sqlite", "external-connections.sqlite", "external-gc.sqlite", "account-quota.sqlite",
    "windows-appearance.json", "startup-appearance.json", ".window-state.json", "remotes", "oauthData.json",
    "content-import-inbox", ".DS_Store", "Thumbs.db", "desktop.ini",
];
/// Temporary files and directories that are renamed or dropped in normal runs.
const DATA_ROOT_TRANSIENT_PREFIXES: &[&str] = &[".tmp", "asset-hydration-", "asset-transient-", "asset-offload-"];
const SQLITE_SIDECARS: &[&str] = &["-wal", "-shm", "-journal"];

/// Every target rather than only the one being compiled, so one table can pin
/// the whole matrix from any host.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Platform {
    Windows,
    MacOs,
    Linux,
    Android,
    Ios,
}

impl Platform {
    pub(crate) fn current() -> Self {
        #[cfg(windows)]
        return Self::Windows;
        #[cfg(target_os = "macos")]
        return Self::MacOs;
        #[cfg(target_os = "linux")]
        return Self::Linux;
        #[cfg(target_os = "android")]
        return Self::Android;
        #[cfg(target_os = "ios")]
        return Self::Ios;
    }

    const fn is_desktop(self) -> bool {
        matches!(self, Self::Windows | Self::MacOs | Self::Linux)
    }
}

/// The OS-owned base directories a layout is built from. This is a flat mapping
/// of the platform accessors; no leaf name may appear here.
#[derive(Clone, Debug, Default)]
pub(crate) struct Hives {
    /// `%APPDATA%`, `~/Library/Application Support`, `$XDG_DATA_HOME`.
    pub roaming: Option<PathBuf>,
    /// `%LOCALAPPDATA%`, `~/Library/Application Support`, `$XDG_DATA_HOME`.
    pub local: Option<PathBuf>,
    /// `%APPDATA%`, `~/Library/Application Support`, `$XDG_CONFIG_HOME`.
    pub config: Option<PathBuf>,
    /// `%LOCALAPPDATA%`, `~/Library/Caches`, `$XDG_CACHE_HOME`.
    pub cache: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// The sandbox directories the OS hands a mobile application.
    pub os_data: Option<PathBuf>,
    pub os_cache: Option<PathBuf>,
    pub os_documents: Option<PathBuf>,
}

impl Hives {
    fn from_platform() -> Self {
        Self {
            roaming: dirs::data_dir(),
            local: dirs::data_local_dir(),
            config: dirs::config_dir(),
            cache: dirs::cache_dir(),
            home: dirs::home_dir(),
            os_data: None,
            os_cache: None,
            os_documents: None,
        }
    }

    fn require(hive: &Option<PathBuf>, name: &str) -> Result<PathBuf, String> {
        hive.clone()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| format!("{name} directory unavailable"))
    }
}

/// XDG roots the Linux desktop-integration files live in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Integration {
    pub data_home: PathBuf,
    pub config_home: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    /// The authoritative native store, with a link at the data root followed.
    pub data: PathBuf,
    /// The data root as the layout names it, before a link at the root itself
    /// is followed. Device secrets are keyed by it, so adding, changing or
    /// removing such a link keeps them.
    pub data_entry: PathBuf,
    /// `None` where the platform manages the WebView profile itself.
    pub webview: Option<PathBuf>,
    pub logs: PathBuf,
    pub cache: PathBuf,
    pub cleanup_control: PathBuf,
    /// Auxiliary Tauri directories and the iOS import Inbox, included in cleanup.
    pub tauri_derived: Vec<PathBuf>,
    /// The directory an uninstall removes. `None` on mobile and for an
    /// AppImage, which is a single file rather than an installed directory.
    pub install: Option<PathBuf>,
    /// Linux only.
    pub integration: Option<Integration>,
}

impl AppPaths {
    /// Resolve the manifest from a configuration alone. Desktop startup and the
    /// cleanup CLI both run before there is an `AppHandle`.
    #[cfg(desktop)]
    pub(crate) fn desktop(config: &tauri::utils::config::Config) -> Result<Self, String> {
        DesktopProfiles::new(config).manifest(None)
    }

    /// Apply the platform checks that turn a computed store root into a usable
    /// one. Kept out of the constructors so the cleanup CLI never creates a
    /// directory it is about to delete.
    #[cfg(desktop)]
    pub(crate) fn prepared(self) -> Result<Self, DataRootError> {
        self.settled(Platform::current(), true)
    }

    /// The same resolution without creating anything, for the cleanup CLI. A
    /// link startup refuses leaves `data` at the link.
    #[cfg(desktop)]
    pub(crate) fn resolved(self) -> Result<Self, String> {
        self.settled(Platform::current(), false).map_err(|error| error.to_string())
    }

    /// Settles `data_entry` and follows a link at it. Without such a link both
    /// keep the path every earlier release used: the layout path on Windows and
    /// macOS, and the canonical path on Linux.
    #[cfg_attr(not(desktop), allow(dead_code))]
    fn settled(mut self, platform: Platform, create: bool) -> Result<Self, DataRootError> {
        let entry = desktop_data_entry(&self.data_entry, platform, create)?;
        self.data = match inspect_data_root(&entry)? {
            DataRoot::Absent if create && platform == Platform::Linux => {
                std::fs::create_dir(&entry)?;
                entry.clone()
            }
            DataRoot::Absent | DataRoot::Directory => entry.clone(),
            DataRoot::Linked(target) => target,
            DataRoot::Refused(refusal) if create => return Err(refusal.into()),
            DataRoot::Refused(_) => entry.clone(),
        };
        self.data_entry = entry;
        self.logs = self.data.join(LOGS_LEAF);
        Ok(self)
    }

    /// Resolve the manifest on mobile, where only the OS knows the sandbox.
    #[cfg(mobile)]
    pub(crate) fn resolve<R: tauri::Runtime>(
        app: &impl tauri::Manager<R>,
    ) -> Result<Self, String> {
        use tauri::Manager;
        let platform = Platform::current();
        let mut hives = Hives::default();
        hives.os_data = app.path().app_data_dir().ok();
        hives.os_cache = app.path().app_cache_dir().ok();
        #[cfg(target_os = "ios")]
        { hives.os_documents = Some(app.path().document_dir().map_err(|_| "application documents directory unavailable")?); }
        let mut paths = Self::layout(platform, &hives, &app.config().identifier, false)?;
        paths.data = platform_store_root(&paths.data, platform)?;
        paths.data_entry = paths.data.clone();
        paths.logs = paths.data.join(LOGS_LEAF);
        paths.cache = platform_store_root(&paths.cache, platform)?;
        paths.cleanup_control = paths
            .data
            .parent()
            .ok_or("application data root has no parent")?
            .join(MOBILE_CLEANUP_LEAF);
        Ok(paths)
    }

    /// Every root inside one directory. An alternative native entry uses this
    /// so that its files never reach the installed product's directories.
    pub fn isolated(root: PathBuf) -> Result<Self, String> {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("an isolated root needs a named directory")?;
        let cleanup_control = root.with_file_name(format!("{name}{CLEANUP_SUFFIX}"));
        Ok(Self {
            logs: root.join(LOGS_LEAF),
            cache: root.join("cache"),
            webview: Some(root.join("webview")),
            data_entry: root.clone(),
            data: root,
            cleanup_control,
            tauri_derived: Vec::new(),
            install: None,
            integration: None,
        })
    }

    /// The whole layout, derived from nothing but the platform, its base
    /// directories, and the identifier. `tests.rs` pins every row of it.
    fn layout(
        platform: Platform,
        hives: &Hives,
        identifier: &str,
        agent: bool,
    ) -> Result<Self, String> {
        validate_identifier(identifier)?;
        if platform == Platform::Ios { Hives::require(&hives.os_documents, "application documents")?; }
        let leaf = |name: &str| {
            if agent && platform.is_desktop() {
                format!("{name}{AGENT_SUFFIX}")
            } else {
                name.to_owned()
            }
        };
        let (data, webview, cache, cleanup_control, integration) = match platform {
            Platform::Windows => {
                let local = Hives::require(&hives.local, "local application data")?;
                (
                    local.join(leaf(WINDOWS_DATA_LEAF)),
                    Some(local.join(leaf(WINDOWS_WEBVIEW_LEAF))),
                    local.join(leaf(WINDOWS_CACHE_LEAF)),
                    local.join(leaf(WINDOWS_CLEANUP_LEAF)),
                    None,
                )
            }
            Platform::MacOs => {
                let support = Hives::require(&hives.roaming, "application support")?;
                let cache = Hives::require(&hives.cache, "cache")?;
                (
                    support.join(leaf(MACOS_DATA_LEAF)),
                    None,
                    cache.join(leaf(MACOS_CACHE_LEAF)),
                    support.join(leaf(MACOS_CLEANUP_LEAF)),
                    None,
                )
            }
            Platform::Linux => {
                let data_home = Hives::require(&hives.roaming, "user data")?;
                let config_home = Hives::require(&hives.config, "user configuration")?;
                let cache = Hives::require(&hives.cache, "cache")?;
                (
                    data_home.join(leaf(LINUX_DATA_LEAF)),
                    Some(data_home.join(leaf(LINUX_WEBVIEW_LEAF))),
                    cache.join(leaf(LINUX_CACHE_LEAF)),
                    data_home.join(leaf(LINUX_CLEANUP_LEAF)),
                    Some(Integration {
                        data_home,
                        config_home,
                    }),
                )
            }
            Platform::Android | Platform::Ios => {
                let os_data = Hives::require(&hives.os_data, "application data")?;
                let data = if platform == Platform::Android { os_data.join("files") } else { os_data };
                let cache = Hives::require(&hives.os_cache, "application cache")?;
                let control = data
                    .parent()
                    .ok_or("application data root has no parent")?
                    .join(MOBILE_CLEANUP_LEAF);
                (data, None, cache, control, None)
            }
        };
        Ok(Self {
            logs: data.join(LOGS_LEAF),
            data_entry: data.clone(),
            data,
            webview,
            cache,
            cleanup_control,
            // An agent build writes to none of them and must not delete the
            // installed product's, which are not leaf-suffixed.
            tauri_derived: if agent && platform.is_desktop() {
                Vec::new()
            } else {
                tauri_derived(platform, hives, identifier)
            },
            install: None,
            integration,
        })
    }

    /// The roots the cleanup operation deletes.
    pub(crate) fn owned_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.data.clone(), self.cache.clone()];
        roots.extend(self.webview.clone());
        roots.extend(self.tauri_derived.iter().cloned());
        roots.sort();
        roots.dedup();
        roots
    }

    /// The first owned root that equals, contains, or sits inside the
    /// installation directory. Deleting such a root can never succeed.
    pub(crate) fn install_conflict(&self) -> Option<PathBuf> {
        let install = self.install.as_deref()?;
        self.owned_roots()
            .into_iter()
            .chain(std::iter::once(self.cleanup_control.clone()))
            .find(|root| overlaps(root, install))
    }
}

/// Every launch profile of one build: the default layout and the named profiles
/// under the build's profile namespace.
#[cfg_attr(not(desktop), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) struct DesktopProfiles {
    platform: Platform,
    hives: Hives,
    identifier: String,
    agent: bool,
    install: Option<PathBuf>,
}

/// Where named profiles live. Each holds one directory per profile name; on
/// macOS and Linux the cache directories sit under a separate base.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProfileBases {
    pub profiles: PathBuf,
    pub caches: Option<PathBuf>,
}

#[cfg_attr(not(desktop), allow(dead_code))]
impl DesktopProfiles {
    #[cfg(desktop)]
    pub(crate) fn new(config: &tauri::utils::config::Config) -> Self {
        let platform = Platform::current();
        Self {
            platform,
            hives: Hives::from_platform(),
            identifier: config.identifier.clone(),
            agent: agent_build(config),
            install: install_directory(platform, appimage_build()),
        }
    }

    /// Every hive inside one directory, for tests that run real cleanups.
    #[cfg(test)]
    pub(crate) fn synthetic(root: &Path, agent: bool) -> Self {
        let hive = |name: &str| Some(root.join(name));
        Self {
            platform: Platform::current(),
            hives: Hives {
                roaming: hive("roaming"), local: hive("local"), config: hive("config"), cache: hive("cache"),
                home: hive("home"), os_data: None, os_cache: None, os_documents: None,
            },
            identifier: "io.github.rsyumi.risunest".to_owned(),
            agent,
            install: None,
        }
    }

    pub(crate) fn agent(&self) -> bool {
        self.agent
    }

    pub(crate) fn identifier(&self) -> &str {
        &self.identifier
    }

    /// The manifest of the default profile, or of a named one.
    pub(crate) fn manifest(&self, profile: Option<&ProfileName>) -> Result<AppPaths, String> {
        let mut paths = match profile {
            None => AppPaths::layout(self.platform, &self.hives, &self.identifier, self.agent)?,
            Some(profile) => self.named(profile)?,
        };
        paths.install = self.install.clone();
        Ok(paths)
    }

    pub(crate) fn bases(&self) -> Result<ProfileBases, String> {
        validate_identifier(&self.identifier)?;
        let leaf = |name: &str| if self.agent { format!("{name}{AGENT_SUFFIX}") } else { name.to_owned() };
        match self.platform {
            Platform::Windows => Ok(ProfileBases {
                profiles: Hives::require(&self.hives.local, "local application data")?.join(leaf(WINDOWS_PROFILES_LEAF)),
                caches: None,
            }),
            Platform::MacOs => Ok(ProfileBases {
                profiles: Hives::require(&self.hives.roaming, "application support")?.join(leaf(MACOS_PROFILES_LEAF)),
                caches: Some(Hives::require(&self.hives.cache, "cache")?.join(leaf(MACOS_PROFILES_LEAF))),
            }),
            Platform::Linux => Ok(ProfileBases {
                profiles: Hives::require(&self.hives.roaming, "user data")?.join(leaf(LINUX_PROFILES_LEAF)),
                caches: Some(Hives::require(&self.hives.cache, "cache")?.join(leaf(LINUX_PROFILES_LEAF))),
            }),
            Platform::Android | Platform::Ios => Err("launch profiles are a desktop feature".into()),
        }
    }

    /// The directories a named profile owns outright: its profile directory and,
    /// where the cache lives elsewhere, its cache directory.
    pub(crate) fn directories(&self, profile: &ProfileName) -> Result<Vec<PathBuf>, String> {
        let bases = self.bases()?;
        let mut directories = vec![bases.profiles.join(profile.as_str())];
        directories.extend(bases.caches.map(|caches| caches.join(profile.as_str())));
        Ok(directories)
    }

    /// The named profiles found on disk. A directory whose name is not a valid
    /// profile name, or that is a link, is not a profile and is never touched.
    pub(crate) fn names(&self) -> Result<Vec<ProfileName>, String> {
        let bases = self.bases()?;
        let mut names = Vec::new();
        for base in std::iter::once(&bases.profiles).chain(bases.caches.as_ref()) {
            let entries = match std::fs::read_dir(base) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("launch profiles unavailable: {error}")),
            };
            for entry in entries {
                let entry = entry.map_err(|error| format!("launch profiles unavailable: {error}"))?;
                let Some(name) = entry.file_name().to_str().and_then(|name| ProfileName::parse(name).ok()) else { continue };
                let metadata = std::fs::symlink_metadata(entry.path())
                    .map_err(|error| format!("launch profiles unavailable: {error}"))?;
                if metadata.is_dir() && !crate::trust_boundary::is_link_like(&metadata) {
                    names.push(name);
                }
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn named(&self, profile: &ProfileName) -> Result<AppPaths, String> {
        let bases = self.bases()?;
        let directory = bases.profiles.join(profile.as_str());
        let data = directory.join(PROFILE_DATA_LEAF);
        Ok(AppPaths {
            logs: data.join(LOGS_LEAF),
            webview: (self.platform != Platform::MacOs).then(|| directory.join(PROFILE_WEBVIEW_LEAF)),
            cache: match bases.caches {
                Some(caches) => caches.join(profile.as_str()),
                None => directory.join(PROFILE_CACHE_LEAF),
            },
            cleanup_control: directory.join(PROFILE_CLEANUP_LEAF),
            data_entry: data.clone(),
            data,
            // Identifier-derived directories and the Linux desktop integration
            // belong to the default profile.
            tauri_derived: Vec::new(),
            install: None,
            integration: None,
        })
    }
}

/// The roots the renderer is allowed to use. It takes them from here instead of
/// deriving any of its own.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Roots {
    data: String,
}

#[tauri::command]
pub(crate) async fn app_paths_roots(app: tauri::AppHandle) -> Result<Roots, String> {
    crate::native_log::logged_without_detail("app_paths_roots", async {
        // The renderer cannot name a staged file before it has a root, so this is
        // where the native side is guaranteed to have received one.
        #[cfg(target_os = "ios")]
        {
            use tauri_plugin_ios_native::IosNativeExt;
            app.ios_native().ensure_data_root().await?;
        }
        let paths = manifest(&app)?;
        Ok(Roots {
            data: paths
                .data
                .to_str()
                .ok_or("application data root is not valid UTF-8")?
                .to_owned(),
        })
    }.await)
}

/// Permit the renderer to reach the manifest roots. The static capability scope
/// cannot name them, because an agent build uses distinct leaves.
pub(crate) fn permit_renderer_access(app: &tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    use tauri_plugin_fs::FsExt;
    let paths = manifest(app)?;
    app.fs_scope()
        .allow_directory(&paths.data, true)
        .map_err(|error| format!("data root file permission unavailable: {error}"))?;
    app.asset_protocol_scope()
        .allow_directory(&paths.data, true)
        .map_err(|error| format!("data root asset permission unavailable: {error}"))
}

/// The resolved manifest. It is managed before any plugin, command, or setup
/// step can run, so an absent one is a programming error rather than a state.
pub(crate) fn manifest<R: tauri::Runtime>(
    app: &impl tauri::Manager<R>,
) -> Result<tauri::State<'_, AppPaths>, String> {
    app.try_state::<AppPaths>()
        .ok_or_else(|| "application paths are unavailable".to_owned())
}

/// The authoritative native store, for the callers that need nothing else.
pub(crate) fn data_root<R: tauri::Runtime>(
    app: &impl tauri::Manager<R>,
) -> Result<PathBuf, String> {
    manifest(app).map(|paths| paths.data.clone())
}

/// The data root's entry path, for the callers that key device secrets by it.
pub(crate) fn data_entry<R: tauri::Runtime>(
    app: &impl tauri::Manager<R>,
) -> Result<PathBuf, String> {
    manifest(app).map(|paths| paths.data_entry.clone())
}

/// A path an earlier run stored below `<data root>/external-storage`, carried
/// over to `root`, which a link at the data root may have moved since. The
/// stored path is returned as it is when it has no such part, so the caller's
/// containment check still rejects it.
pub(crate) fn relocated_external_storage_path(stored: &Path, root: &Path) -> PathBuf {
    use std::path::Component;
    let components = stored.components().collect::<Vec<_>>();
    let Some(anchor) = components.iter().rposition(|component| component.as_os_str() == "external-storage") else {
        return stored.to_owned();
    };
    let rest = &components[anchor + 1..];
    if rest.is_empty() || !rest.iter().all(|component| matches!(component, Component::Normal(_))) {
        return stored.to_owned();
    }
    rest.iter().fold(root.join("external-storage"), |path, component| path.join(component))
}

/// Whether a top-level name inside the data root is one the app creates.
#[cfg_attr(not(desktop), allow(dead_code))]
pub(crate) fn is_data_root_entry(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else { return false };
    let base = SQLITE_SIDECARS
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix).filter(|base| base.ends_with(".sqlite")))
        .unwrap_or(name);
    DATA_ROOT_ENTRIES.contains(&base)
        || DATA_ROOT_TRANSIENT_PREFIXES.iter().any(|prefix| name.len() > prefix.len() && name.starts_with(prefix))
}

/// Component-wise containment. A string prefix would report `RisuNestData` as
/// sitting inside `RisuNest`.
fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn tauri_derived(platform: Platform, hives: &Hives, identifier: &str) -> Vec<PathBuf> {
    let mut derived = Vec::new();
    match platform {
        Platform::Windows | Platform::MacOs | Platform::Linux => {
            for hive in [&hives.roaming, &hives.local, &hives.config, &hives.cache] {
                if let Some(hive) = hive.as_deref().filter(|path| path.is_absolute()) {
                    derived.push(hive.join(identifier));
                }
            }
            if platform == Platform::MacOs {
                if let Some(home) = hives.home.as_deref().filter(|path| path.is_absolute()) {
                    derived.push(home.join("Library/Logs").join(identifier));
                }
            }
        }
        Platform::Ios => {
            if let Some(documents) = hives.os_documents.as_deref().filter(|path| path.is_absolute()) {
                derived.push(documents.join("Inbox"));
            }
        }
        Platform::Android => {}
    }
    derived.sort();
    derived.dedup();
    derived
}

/// The installation directory each platform's installer selects. Used by the
/// matrix test; `install` itself comes from the running executable.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn default_install_dirs(platform: Platform, hives: &Hives) -> Vec<PathBuf> {
    match platform {
        Platform::Windows => hives
            .local
            .iter()
            .map(|local| local.join(WINDOWS_INSTALL_LEAF))
            .collect(),
        Platform::MacOs => vec![PathBuf::from(MACOS_INSTALL_DIR)],
        Platform::Linux => LINUX_INSTALL_DIRS.iter().map(PathBuf::from).collect(),
        Platform::Android | Platform::Ios => Vec::new(),
    }
}

#[cfg(desktop)]
fn install_directory(platform: Platform, appimage: bool) -> Option<PathBuf> {
    if !platform.is_desktop() || appimage {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    if platform == Platform::MacOs {
        // A macOS uninstall removes the bundle, not the directory holding the
        // executable inside it.
        if let Some(bundle) = directory
            .ancestors()
            .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        {
            return Some(bundle.to_owned());
        }
    }
    Some(directory.to_owned())
}

#[cfg(desktop)]
fn appimage_build() -> bool {
    #[cfg(target_os = "linux")]
    return std::env::var_os("APPIMAGE").is_some();
    #[cfg(not(target_os = "linux"))]
    return false;
}

#[cfg(desktop)]
fn agent_build(config: &tauri::utils::config::Config) -> bool {
    config
        .plugins
        .0
        .get("risunest")
        .and_then(|value| value.get("agent"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn validate_identifier(identifier: &str) -> Result<(), String> {
    let valid = !identifier.is_empty()
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    valid
        .then_some(())
        .ok_or_else(|| "application identifier is invalid".to_owned())
}

/// Android mount aliases are trusted only above the OS-provided root. CAS still
/// rejects every app-owned link.
#[cfg(any(mobile, test))]
fn platform_store_root(root: &Path, platform: Platform) -> Result<PathBuf, String> {
    if platform != Platform::Android {
        return Ok(root.to_owned());
    }
    resolve_platform_root(root, true)
        .map_err(|error| format!("application data root unavailable: {error}"))
}

/// Where a desktop data root leads, seen from its entry path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DataRoot {
    Absent,
    Directory,
    /// The entry is a link to this directory, every link in it followed.
    Linked(PathBuf),
    /// The entry is a link startup does not follow.
    Refused(LinkRefusal),
}

/// Why startup does not follow a data root link. Cleanup removes such a link
/// without opening what it leads to, since no data root is kept there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkRefusal {
    TargetMissing,
    /// The store's SQLite WAL mode does not work on a network file system.
    NetworkTarget,
    NotDirectory,
    /// A Windows target whose name has no form without the `\\?\` prefix.
    #[cfg_attr(not(windows), allow(dead_code))]
    NoPlainPath,
}

impl std::fmt::Display for LinkRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TargetMissing => "Application data root link target is missing",
            Self::NetworkTarget => "Application data root link must lead to a local drive",
            Self::NotDirectory => "Application data root link must lead to a directory",
            Self::NoPlainPath => "Application data root link target has no plain path",
        })
    }
}

/// Why startup cannot use the desktop data root.
#[cfg_attr(not(desktop), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct DataRootError {
    message: String,
    /// Set when the data root is a link the user has to repair.
    pub(crate) refusal: Option<LinkRefusal>,
}

impl From<std::io::Error> for DataRootError {
    fn from(error: std::io::Error) -> Self {
        Self { message: error.to_string(), refusal: None }
    }
}

impl From<LinkRefusal> for DataRootError {
    fn from(refusal: LinkRefusal) -> Self {
        Self { message: refusal.to_string(), refusal: Some(refusal) }
    }
}

impl std::fmt::Display for DataRootError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "application data root unavailable: {}", self.message)
    }
}

impl std::error::Error for DataRootError {}

/// The entry path of a desktop data root. Linux has always resolved the
/// aliases above the leaf, so its parent is canonical; Windows and macOS keep
/// the layout path exactly as computed.
fn desktop_data_entry(layout: &Path, platform: Platform, create: bool) -> std::io::Result<PathBuf> {
    use std::io;
    if !layout.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Application data root must be absolute"));
    }
    if platform != Platform::Linux {
        return Ok(layout.to_owned());
    }
    let (Some(parent), Some(leaf)) = (layout.parent(), layout.file_name()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Application data root has no parent"));
    };
    if create {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::canonicalize(parent) {
        Ok(parent) => Ok(parent.join(leaf)),
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => Ok(layout.to_owned()),
        Err(error) => Err(error),
    }
}

/// Follows a link at the data root itself. Only a target that is a directory
/// on a local drive is accepted.
pub(crate) fn inspect_data_root(entry: &Path) -> std::io::Result<DataRoot> {
    use std::{fs, io};
    let metadata = match fs::symlink_metadata(entry) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DataRoot::Absent),
        result => result?,
    };
    if !crate::trust_boundary::is_link_like(&metadata) {
        return if metadata.is_dir() {
            Ok(DataRoot::Directory)
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, "Application data root must be a directory"))
        };
    }
    let target = match fs::canonicalize(entry) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DataRoot::Refused(LinkRefusal::TargetMissing)),
        result => match local_path(result?)? {
            Ok(target) => target,
            Err(refusal) => return Ok(DataRoot::Refused(refusal)),
        },
    };
    if !fs::metadata(&target)?.is_dir() {
        return Ok(DataRoot::Refused(LinkRefusal::NotDirectory));
    }
    Ok(DataRoot::Linked(target))
}

/// A canonical Windows path without the `\\?\` prefix, which the WebView, the
/// renderer and the path checks elsewhere do not expect. A network share is
/// refused.
#[cfg(windows)]
fn local_path(path: PathBuf) -> std::io::Result<Result<PathBuf, LinkRefusal>> {
    use std::{io, path::{Component, Prefix}};
    let mut components = path.components();
    let local = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) => return Ok(Ok(path)),
            Prefix::VerbatimDisk(drive) => {
                let mut local = PathBuf::from(format!("{}:\\", char::from(drive)));
                local.extend(components.filter(|component| matches!(component, Component::Normal(_))));
                local
            }
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => return Ok(Err(LinkRefusal::NetworkTarget)),
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => return Ok(Err(LinkRefusal::NoPlainPath)),
        },
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "Application data root must be absolute")),
    };
    // A name the plain form cannot express would lead somewhere else.
    if std::fs::canonicalize(&local)? != path {
        return Ok(Err(LinkRefusal::NoPlainPath));
    }
    Ok(Ok(local))
}

/// A target on a network file system is refused.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn local_path(path: PathBuf) -> std::io::Result<Result<PathBuf, LinkRefusal>> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `name` is NUL-terminated and statfs fills `stat` when it succeeds.
    if unsafe { libc::statfs(name.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: statfs succeeded above.
    let stat = unsafe { stat.assume_init() };
    #[cfg(target_os = "macos")]
    let network = network_mount_flags(stat.f_flags);
    #[cfg(target_os = "linux")]
    let network = network_file_system(stat.f_type as u32);
    Ok(if network { Err(LinkRefusal::NetworkTarget) } else { Ok(path) })
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn local_path(path: PathBuf) -> std::io::Result<Result<PathBuf, LinkRefusal>> {
    Ok(Ok(path))
}

/// macOS marks every mount it does not consider local, network shares and
/// unmarked FUSE mounts alike.
#[cfg(any(target_os = "macos", test))]
fn network_mount_flags(flags: u32) -> bool {
    flags & 0x0000_1000 == 0 // MNT_LOCAL
}

/// The Linux `statfs` magic numbers of network file systems. FUSE and 9p are
/// left out because local file systems use them too.
#[cfg(any(target_os = "linux", test))]
fn network_file_system(magic: u32) -> bool {
    matches!(magic,
        0x6969 // NFS
        | 0x517B // SMB
        | 0xFF53_4D42 // CIFS
        | 0xFE53_4D42 // SMB2
        | 0x5346_414F | 0x6B41_4653 // AFS
        | 0x7375_7245 // Coda
        | 0x564C // NCP
        | 0x00C3_6400 // Ceph
    )
}

/// The main window when startup must build it itself: with the WebView data
/// directory where the platform takes one, or with a named WebKit data store.
pub(crate) struct MainWindow {
    pub config: tauri::utils::config::WindowConfig,
    pub data_directory: Option<PathBuf>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub data_store: Option<[u8; 16]>,
}

#[cfg(desktop)]
pub(crate) fn take_main_window(
    context: &mut tauri::Context<tauri::Wry>,
    paths: &AppPaths,
    data_store: Option<[u8; 16]>,
) -> std::io::Result<Option<MainWindow>> {
    // Otherwise the configuration builds it, as it does for the default
    // profile on macOS.
    if paths.webview.is_none() && data_store.is_none() {
        return Ok(None);
    }
    let index = context
        .config_mut()
        .app
        .windows
        .iter()
        .position(|window| window.label == "main")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "main window missing")
        })?;
    let config = context.config_mut().app.windows.remove(index);
    Ok(Some(MainWindow { config, data_directory: paths.webview.clone(), data_store }))
}

#[cfg(any(mobile, test))]
fn resolve_platform_root(root: &Path, create_missing: bool) -> std::io::Result<PathBuf> {
    use std::{fs, io};

    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Application data root must be absolute",
        ));
    }
    let metadata = match fs::symlink_metadata(root) {
        Err(error) if create_missing && error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            fs::symlink_metadata(root)?
        }
        result => result?,
    };
    if crate::trust_boundary::is_link_like(&metadata) || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Application data root must be a real directory",
        ));
    }

    fs::canonicalize(root)
}

#[cfg(test)]
pub(crate) mod tests;
