mod files;
mod paths;
pub(crate) mod mobile;

use files::{failed, Result};
#[cfg(desktop)]
use crate::{app_paths::DesktopProfiles, launch_profile::ProfileName};
use crate::native_log::logged;
use paths::Paths;
use serde::{Deserialize, Serialize};
use std::{fs, sync::atomic::{AtomicBool, Ordering}};
#[cfg(desktop)]
use std::time::{Duration, Instant};
#[cfg(test)]
use std::path::Path;
#[cfg(mobile)]
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

/// `PrepareRemoval` covers every launch profile; `RemoveProfile` covers only the
/// profile whose journal holds it, so an interrupted single-profile removal can
/// never resume as a wider one.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Mode { Reset, PrepareRemoval, RemoveProfile }

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    token: String,
    mode: Mode,
    webview_cleared: bool,
    roots_started: bool,
    error: Option<String>,
    appimage: Option<std::path::PathBuf>,
}

fn read_request(paths: &Paths) -> Result<Option<Request>> {
    files::validate(&paths.request())?;
    let bytes = match fs::read(paths.request()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed("cleanup-journal-unavailable", error)),
    };
    let request: Request = serde_json::from_slice(&bytes).map_err(|error| failed("cleanup-journal-corrupt", error))?;
    if request.version != 1 || !uuid::Uuid::parse_str(&request.token)
        .is_ok_and(|value| value.get_version_num() == 4 && value.to_string() == request.token) {
        return Err("cleanup-journal-corrupt".into());
    }
    Ok(Some(request))
}

pub(crate) struct CleanupState {
    paths: Paths,
    #[cfg(desktop)]
    profile: Option<ProfileName>,
    /// The other profiles of the product layout. An entry that manages its own
    /// roots has none, so its cleanup can never reach the product's profiles.
    #[cfg(desktop)]
    profiles: Option<DesktopProfiles>,
    unavailable: Option<String>,
    pending: AtomicBool,
    running: AtomicBool,
    closing: AtomicBool,
    #[cfg(desktop)]
    _owner: Option<fs::File>,
    #[cfg(mobile)]
    maintenance: Mutex<Option<Maintenance>>,
    #[cfg(mobile)]
    ready: AtomicBool,
}

#[cfg(mobile)]
struct Maintenance {
    _native: crate::native_file_jobs::admission::Permit,
    _renderer: crate::persistent_store::commands::DeviceMaintenanceGuard,
}

impl CleanupState {
    pub(crate) fn initialize(app: &AppHandle) -> Result<Self> {
        let paths = Paths::from_app(app)?;
        #[allow(unused_mut)]
        let mut state = Self::from_paths(paths)?;
        #[cfg(desktop)]
        {
            state.profile = crate::launch_profile::current(app);
            let profiles = DesktopProfiles::new(app.config());
            let product = profiles.manifest(state.profile.as_ref())
                .is_ok_and(|manifest| manifest.cleanup_control == state.paths.control);
            state.profiles = product.then_some(profiles);
        }
        Ok(state)
    }

    fn from_paths(paths: Paths) -> Result<Self> {
        let unavailable = paths.preflight().err();
        #[cfg(desktop)]
        let owner = if files::validate(&paths.control).is_ok() { Some(lock(&paths, Duration::ZERO)?) } else { None };
        let pending = if unavailable.is_some() {
            !matches!(fs::symlink_metadata(paths.request()), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        } else { !matches!(read_request(&paths), Ok(None)) };
        Ok(Self { paths, unavailable, pending: AtomicBool::new(pending), running: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            #[cfg(desktop)] profile: None,
            #[cfg(desktop)] profiles: None,
            #[cfg(desktop)] _owner: owner,
            #[cfg(mobile)] maintenance: Mutex::new(None),
            #[cfg(mobile)] ready: AtomicBool::new(false),
        })
    }

    fn available(&self) -> Result<()> {
        if let Some(error) = &self.unavailable { return Err(error.clone()); }
        self.paths.preflight()
    }

    /// The selected profile's name and whether the build has named profiles, so
    /// the settings screen can say which data a reset or removal covers.
    fn profile_status(&self) -> (Option<String>, bool) {
        #[cfg(desktop)]
        return (
            self.profile.as_ref().map(|profile| profile.as_str().to_owned()),
            self.profiles.as_ref().and_then(|profiles| profiles.names().ok()).is_some_and(|names| !names.is_empty()),
        );
        #[cfg(mobile)]
        return (None, false);
    }
}

pub(crate) fn pending(app: &AppHandle) -> bool {
    app.try_state::<CleanupState>().is_some_and(|state| state.pending.load(Ordering::Acquire))
}

pub(crate) fn closing(app: &AppHandle) -> bool {
    app.try_state::<CleanupState>().is_some_and(|state| state.closing.load(Ordering::Acquire))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
    pending: bool, mode: Option<Mode>, error: Option<String>, can_cancel: bool,
    profile: Option<String>, named_profiles: bool, cleanup_folder: String,
}

#[tauri::command]
pub(crate) fn app_cleanup_status(app: AppHandle) -> Result<Status> {
    logged("app_cleanup_status", (|| {
        let state = app.try_state::<CleanupState>().ok_or("cleanup-state-unavailable")?;
        let (profile, named_profiles) = state.profile_status();
        let status = |pending, mode, error, can_cancel| Status { pending, mode, error, can_cancel, profile: profile.clone(), named_profiles, cleanup_folder: state.paths.control.to_string_lossy().into_owned() };
        if let Some(error) = &state.unavailable {
            return Ok(status(state.pending.load(Ordering::Acquire), None, Some(error.clone()), false));
        }
        #[cfg(mobile)]
        if state.ready.load(Ordering::Acquire) {
            // The fresh document hands storage back to normal bootstrap.
            mobile::finish(&app)?;
            fs::remove_file(state.paths.request()).map_err(|error| failed("cleanup-journal-unavailable", error))?;
            let _ = fs::remove_dir(&state.paths.control);
            state.pending.store(false, Ordering::Release);
            state.ready.store(false, Ordering::Release);
        }
        match read_request(&state.paths) {
            Ok(Some(request)) => Ok(status(true, Some(request.mode), request.error,
                !request.roots_started && !state.running.load(Ordering::Acquire) && !state.closing.load(Ordering::Acquire))),
            Ok(None) => Ok(status(false, None, None, false)),
            Err(error) => Ok(status(true, None, Some(error), false)),
        }
    })())
}

fn navigate(app: &AppHandle) -> Result<()> {
    let window = app.get_webview_window("main").ok_or("cleanup-window-unavailable")?;
    let mut url = window.url().map_err(|error| failed("cleanup-window-unavailable", error))?;
    url.set_query(None);
    url.set_fragment(None);
    window.navigate(url).map_err(|error| failed("cleanup-navigation-failed", error))
}

#[tauri::command]
pub(crate) async fn app_cleanup_request(app: AppHandle, mode: Mode) -> Result<()> {
    logged("app_cleanup_request", async move {
        // Only the command line removes a single profile.
        if mode == Mode::RemoveProfile { return Err("cleanup-mode-unavailable".into()); }
        #[cfg(mobile)]
        if mode == Mode::PrepareRemoval { return Err("cleanup-mode-unavailable".into()); }
        let state = app.state::<CleanupState>();
        state.available()?;
        #[cfg(desktop)]
        if mode == Mode::PrepareRemoval {
            if let Some(profiles) = &state.profiles {
                refuse_while_other_profiles_run(profiles, state.profile.as_ref())?;
            }
        }
        crate::cleanup_webview::preflight(&app).await?;
        if state.pending.swap(true, Ordering::AcqRel) { return Err("cleanup-already-pending".into()); }
        let request = Request {
            version: 1, token: uuid::Uuid::new_v4().to_string(), mode,
            webview_cleared: false, roots_started: false, error: None,
            #[cfg(target_os = "linux")]
            appimage: app.env().appimage.map(std::path::PathBuf::from),
            #[cfg(not(target_os = "linux"))]
            appimage: None,
        };
        if let Err(error) = files::write_json(&state.paths.request(), &request) {
            state.pending.store(false, Ordering::Release);
            return Err(error);
        }
        navigate(&app)
    }.await)
}

struct Running<'a>(&'a AtomicBool);
impl Drop for Running<'_> { fn drop(&mut self) { self.0.store(false, Ordering::Release); } }

#[tauri::command]
pub(crate) async fn app_cleanup_resume(app: AppHandle) -> Result<()> {
    logged("app_cleanup_resume", async move {
        let state = app.state::<CleanupState>();
        state.available()?;
        if state.running.swap(true, Ordering::AcqRel) { return Err("cleanup-already-running".into()); }
        let _running = Running(&state.running);
        let mut request = read_request(&state.paths)?.ok_or("cleanup-not-pending")?;
        let result = resume(&app, &mut request).await;
        if let Err(error) = &result {
            if let Ok(Some(mut recorded)) = read_request(&state.paths) {
                recorded.error = Some(error.clone());
                let _ = files::write_json(&state.paths.request(), &recorded);
            }
        }
        result
    }.await)
}

fn cancellable_request(paths: &Paths) -> Result<Request> {
    paths.preflight()?;
    let request = read_request(paths)?.ok_or("cleanup-not-pending")?;
    if request.roots_started { return Err("cleanup-cancel-unavailable".into()); }
    Ok(request)
}

fn remove_request(paths: &Paths) -> Result<()> {
    fs::remove_file(paths.request()).map_err(|error| failed("cleanup-journal-unavailable", error))?;
    #[cfg(unix)]
    fs::File::open(&paths.control).and_then(|directory| directory.sync_all())
        .map_err(|error| failed("cleanup-journal-unavailable", error))?;
    Ok(())
}

async fn cancel_with(paths: &Paths, restore: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    cancellable_request(paths)?;
    restore.await?;
    remove_request(paths)
}

#[tauri::command]
pub(crate) async fn app_cleanup_cancel(app: AppHandle) -> Result<()> {
    logged("app_cleanup_cancel", async move {
        let state = app.state::<CleanupState>();
        state.available()?;
        if state.closing.load(Ordering::Acquire) { return Err("cleanup-cancel-unavailable".into()); }
        if state.running.swap(true, Ordering::AcqRel) { return Err("cleanup-already-running".into()); }
        let _running = Running(&state.running);
        let result = cancel_with(&state.paths, async {
            #[cfg(mobile)]
            {
                // Complete any partial teardown before reopening the retained library.
                mobile::prepare(&app).await?;
                mobile::rebuild(&app)?;
                mobile::finish(&app)?;
            }
            Ok(())
        }).await;
        if matches!(read_request(&state.paths), Ok(None)) {
            state.pending.store(false, Ordering::Release);
        }
        result?;
        navigate(&app)
    }.await)
}

async fn resume(app: &AppHandle, request: &mut Request) -> Result<()> {
    crate::cleanup_webview::preflight(app).await?;
    #[cfg(mobile)]
    mobile::prepare(app).await?;
    crate::cleanup_webview::clear(app).await?;
    let state = app.state::<CleanupState>();
    request.webview_cleared = true;
    request.error = None;
    files::write_json(&state.paths.request(), request)?;
    #[cfg(desktop)]
    {
        let executable = std::env::current_exe().map_err(|error| failed("cleanup-executable-unavailable", error))?;
        let mut command = std::process::Command::new(executable);
        command.args(["--cleanup-worker", &request.token]);
        if let Some(profile) = &state.profile { command.arg(profile.argument()); }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()).spawn().map_err(|error| failed("cleanup-worker-start-failed", error))?;
        if let Some(lock) = app.try_state::<DataRootLock>() { lock.release(); }
        state.closing.store(true, Ordering::Release);
        app.exit(0);
        Ok(())
    }
    #[cfg(mobile)]
    {
        let paths = state.paths.clone();
        let mut worker_request = request.clone();
        let (updated, result) = tauri::async_runtime::spawn_blocking(move || {
            let result = erase(&paths, &mut worker_request);
            (worker_request, result)
        }).await.map_err(|error| failed("cleanup-worker-failed", error))?;
        *request = updated;
        result?;
        mobile::rebuild(app)?;
        state.ready.store(true, Ordering::Release);
        if let Err(error) = navigate(app) {
            state.ready.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
}

fn begin_roots(paths: &Paths, request: &mut Request) -> Result<()> {
    request.roots_started = true;
    files::write_json(&paths.request(), request)
}

#[cfg_attr(desktop, allow(dead_code))]
fn erase(paths: &Paths, request: &mut Request) -> Result<()> {
    erase_with(paths, request, &crate::cleanup_secrets::remove_all, files::remove)
}

fn erase_with(paths: &Paths, request: &mut Request,
    remove_secrets: &dyn Fn(&std::path::Path) -> Result<()>,
    mut remove_root: impl FnMut(&std::path::Path) -> Result<()>) -> Result<()> {
    paths.preflight()?;
    #[cfg(desktop)]
    if let Some(directory) = paths.data_directory() { ensure_data_root_unused(directory)?; }
    remove_secrets(&paths.data)?;
    begin_roots(paths, request)?;
    for root in &paths.roots { remove_root(root)?; }
    // A reset keeps the link, so the relaunched app goes on using its target.
    if let Some(link) = &paths.data_link {
        files::clear_linked(&link.link, link.target.as_deref(), request.mode != Mode::Reset)?;
    }
    Ok(())
}

/// Held by a desktop instance inside the data root it uses for its whole run,
/// so that another profile whose data root leads to the same directory can
/// neither open it nor delete it meanwhile.
#[cfg(desktop)]
pub(crate) struct DataRootLock(std::sync::Mutex<Option<fs::File>>);

#[cfg(desktop)]
impl DataRootLock {
    pub(crate) fn acquire(root: &std::path::Path) -> Result<Self> {
        fs::create_dir_all(root).map_err(|error| failed("data-root-lock-unavailable", error))?;
        let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(root.join(crate::app_paths::DATA_ROOT_LOCK)).map_err(|error| failed("data-root-lock-unavailable", error))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self(std::sync::Mutex::new(Some(file)))),
            Err(error) if contended(&error) => Err("data-root-in-use".into()),
            Err(error) => Err(failed("data-root-lock-unavailable", error)),
        }
    }

    /// Lets the cleanup worker in before the owner lock goes with the process,
    /// whose handles close in no particular order.
    fn release(&self) {
        drop(self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take());
    }
}

/// Only an existing lock file is opened, so the check creates nothing.
#[cfg(desktop)]
fn ensure_data_root_unused(root: &std::path::Path) -> Result<()> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(root.join(crate::app_paths::DATA_ROOT_LOCK)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed("cleanup-lock-unavailable", error)),
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(()),
        Err(error) if contended(&error) => Err("cleanup-data-root-in-use".into()),
        Err(error) => Err(failed("cleanup-lock-unavailable", error)),
    }
}

/// The effects of a desktop cleanup outside the profile directories.
#[cfg(desktop)]
struct Effects<'a> {
    remove_secrets: &'a dyn Fn(&std::path::Path) -> Result<()>,
    remove_webview: &'a dyn Fn(Option<&ProfileName>) -> Result<()>,
    relaunch: &'a dyn Fn(&Request, Option<&ProfileName>) -> Result<()>,
}

/// A lock attempt that failed only because another process holds the lock.
#[cfg(desktop)]
fn contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error().is_some() && error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

#[cfg(desktop)]
fn lock(paths: &Paths, timeout: Duration) -> Result<fs::File> {
    files::validate(&paths.control)?;
    fs::create_dir_all(&paths.control).map_err(|error| failed("cleanup-journal-unavailable", error))?;
    let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(paths.control.join("owner.lock")).map_err(|error| failed("cleanup-lock-unavailable", error))?;
    let deadline = Instant::now() + timeout;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if contended(&error) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(failed("cleanup-app-still-running", error)),
        }
    }
}

/// Whether a profile's owner lock is held. Only an existing lock file is
/// opened, so checking another profile creates nothing inside it.
#[cfg(desktop)]
fn owner_running(paths: &Paths) -> Result<bool> {
    files::validate(&paths.control)?;
    let file = match fs::OpenOptions::new().read(true).write(true).open(paths.control.join("owner.lock")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(failed("cleanup-lock-unavailable", error)),
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(false),
        Err(error) if contended(&error) => Ok(true),
        Err(error) => Err(failed("cleanup-lock-unavailable", error)),
    }
}

/// Every profile other than `profile`, the default one first.
#[cfg(desktop)]
fn other_profiles(profiles: &DesktopProfiles, profile: Option<&ProfileName>) -> Result<Vec<Option<ProfileName>>> {
    let names = profiles.names().map_err(|error| failed("cleanup-profiles-unavailable", error))?;
    Ok(profile.map(|_| None).into_iter()
        .chain(names.into_iter().filter(|name| Some(name) != profile).map(Some))
        .collect())
}

#[cfg(desktop)]
fn profile_paths(profiles: &DesktopProfiles, profile: Option<&ProfileName>) -> Result<Paths> {
    let manifest = profiles.manifest(profile).and_then(crate::app_paths::AppPaths::resolved)
        .map_err(|error| failed("cleanup-path-unavailable", error))?;
    Ok(Paths::from_manifest(&manifest))
}

/// Prepare app removal deletes every profile, so it refuses to start while
/// another one runs instead of deleting data that profile still uses.
#[cfg(desktop)]
fn refuse_while_other_profiles_run(profiles: &DesktopProfiles, profile: Option<&ProfileName>) -> Result<()> {
    for other in other_profiles(profiles, profile)? {
        if owner_running(&profile_paths(profiles, other.as_ref())?)? {
            return Err("cleanup-other-profiles-running".into());
        }
    }
    Ok(())
}

#[cfg(desktop)]
fn erase_retrying(paths: &Paths, request: &mut Request, effects: &Effects) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match erase_with(paths, request, effects.remove_secrets, files::remove) {
            Ok(()) => return Ok(()),
            Err(error) if error == "cleanup-files-busy-or-denied" && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(error),
        }
    }
}

/// Once nothing of a named profile remains, its directories go, and so do the
/// profile namespaces when they are empty.
#[cfg(desktop)]
fn remove_profile_directories(profiles: &DesktopProfiles, profile: Option<&ProfileName>) -> Result<()> {
    if let Some(profile) = profile {
        for directory in profiles.directories(profile).map_err(|error| failed("cleanup-path-unavailable", error))? {
            files::remove(&directory)?;
        }
    }
    if let Ok(bases) = profiles.bases() {
        for base in std::iter::once(bases.profiles).chain(bases.caches) {
            let _ = fs::remove_dir(base);
        }
    }
    Ok(())
}

/// Erases a profile other than the one holding the removal request, under its
/// own owner lock and its own `RemoveProfile` journal. An interruption leaves it
/// pending cleanup rather than started without its credentials.
#[cfg(desktop)]
fn erase_other(profiles: &DesktopProfiles, profile: Option<&ProfileName>, effects: &Effects) -> Result<()> {
    let paths = profile_paths(profiles, profile)?;
    let owner = lock(&paths, Duration::from_secs(30))?;
    paths.preflight()?;
    let mut request = read_request(&paths)?.unwrap_or_else(|| new_request(Mode::RemoveProfile));
    // No WebView of this profile is running, and its WebView data goes with it.
    request.mode = Mode::RemoveProfile;
    request.webview_cleared = true;
    files::write_json(&paths.request(), &request)?;
    let result = erase_retrying(&paths, &mut request, effects)
        .and_then(|()| (effects.remove_webview)(profile));
    if let Err(error) = result {
        request.error = Some(error.clone());
        let _ = files::write_json(&paths.request(), &request);
        return Err(error);
    }
    drop(owner);
    files::remove(&paths.control)?;
    remove_profile_directories(profiles, profile)
}

#[cfg(desktop)]
fn new_request(mode: Mode) -> Request {
    Request {
        version: 1, token: uuid::Uuid::new_v4().to_string(), mode,
        webview_cleared: false, roots_started: false, error: None, appimage: None,
    }
}

/// Erases what the request covers, the profile holding the request last, then
/// its journal and, for a removal, its directories.
#[cfg(desktop)]
fn remove(profiles: &DesktopProfiles, profile: Option<&ProfileName>, paths: &Paths, request: &mut Request,
    owner: fs::File, effects: &Effects) -> Result<()> {
    let result = (|| -> Result<()> {
        if request.mode == Mode::PrepareRemoval {
            for other in other_profiles(profiles, profile)? {
                erase_other(profiles, other.as_ref(), effects)?;
            }
        }
        erase_retrying(paths, request, effects)?;
        if request.mode != Mode::Reset {
            (effects.remove_webview)(profile)?;
        }
        if request.mode == Mode::PrepareRemoval {
            let manifest = profiles.manifest(None).map_err(|error| failed("cleanup-path-unavailable", error))?;
            remove_integration(&manifest, request)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        request.error = Some(error.clone());
        let _ = files::write_json(&paths.request(), request);
        return Err(error);
    }
    drop(owner);
    files::remove(&paths.control)?;
    if request.mode == Mode::Reset { Ok(()) } else { remove_profile_directories(profiles, profile) }
}

#[cfg(desktop)]
pub(crate) fn run_cli(arguments: &[std::ffi::OsString], profiles: &DesktopProfiles, profile: Option<&ProfileName>) -> Option<i32> {
    let args = arguments.iter().map(|argument| argument.to_string_lossy().into_owned()).collect::<Vec<_>>();
    if !args.first().is_some_and(|arg| matches!(arg.as_str(), "--remove-local-data" | "--cleanup-worker")) { return None; }
    let remove_webview = |profile: Option<&ProfileName>| crate::cleanup_webview::remove_profile_store(profiles.agent(), profile);
    let effects = Effects { remove_secrets: &crate::cleanup_secrets::remove_all, remove_webview: &remove_webview, relaunch: &relaunch };
    match cli(&args, profiles, profile, &effects) {
        Ok(()) => Some(0),
        Err(error) => { eprintln!("{error}"); Some(1) },
    }
}

#[cfg(desktop)]
fn cli(args: &[String], profiles: &DesktopProfiles, profile: Option<&ProfileName>, effects: &Effects) -> Result<()> {
    let paths = profile_paths(profiles, profile)?;
    match args {
        [command, yes] if command == "--remove-local-data" && yes == "--yes" => {
            // Without a profile the command removes every profile, as the
            // Windows uninstaller expects; with one it removes only that one.
            let mode = if profile.is_some() { Mode::RemoveProfile } else { Mode::PrepareRemoval };
            if let Some(profile) = profile {
                let directories = profiles.directories(profile).map_err(|error| failed("cleanup-path-unavailable", error))?;
                if !directories.iter().any(|directory| fs::symlink_metadata(directory).is_ok()) { return Ok(()); }
            }
            let owner = lock(&paths, Duration::from_secs(30))?;
            paths.preflight()?;
            let mut request = read_request(&paths)?.unwrap_or_else(|| new_request(mode));
            request.mode = mode;
            files::write_json(&paths.request(), &request)?;
            remove(profiles, profile, &paths, &mut request, owner, effects)
        },
        [command, token] if command == "--cleanup-worker" => {
            let owner = lock(&paths, Duration::from_secs(60))?;
            let mut request = read_request(&paths)?.ok_or("cleanup-not-pending")?;
            if request.token != *token || !request.webview_cleared { return Err("cleanup-request-mismatch".into()); }
            match remove(profiles, profile, &paths, &mut request, owner, effects) {
                Err(error) => {
                    let _ = (effects.relaunch)(&request, profile);
                    Err(error)
                },
                Ok(()) if request.mode == Mode::Reset => (effects.relaunch)(&request, profile),
                Ok(()) => Ok(()),
            }
        },
        _ => Err("cleanup-confirmation-required".into()),
    }
}

#[cfg(desktop)]
fn relaunch(request: &Request, profile: Option<&ProfileName>) -> Result<()> {
    let executable = match &request.appimage {
        Some(path) => path.clone(),
        None => std::env::current_exe().map_err(|error| failed("cleanup-executable-unavailable", error))?,
    };
    let mut command = std::process::Command::new(executable);
    if let Some(profile) = profile { command.arg(profile.argument()); }
    command.spawn().map_err(|error| failed("cleanup-relaunch-failed", error))?;
    Ok(())
}

#[cfg(desktop)]
fn remove_integration(manifest: &crate::app_paths::AppPaths, request: &Request) -> Result<()> {
    #[cfg(target_os = "linux")]
    if let (Some(image), Some(integration)) = (&request.appimage, manifest.integration.as_ref()) {
        return crate::appimage_integration::remove_owned(image, integration);
    }
    let _ = (manifest, request);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request { version: 1, token: uuid::Uuid::new_v4().to_string(), mode: Mode::Reset,
            webview_cleared: true, roots_started: false, error: None, appimage: None }
    }
    fn paths(root: &Path) -> Paths {
        Paths { data: root.join("data"), roots: vec![root.join("data"), root.join("webview")], data_link: None, control: root.join("control"), install_conflict: None }
    }
    #[test]
    fn full_reset_removes_profile_image_geometry() {
        use crate::persistent_store::{PersistentStore, image_geometry::{self, ImageGeometry}};
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let store = PersistentStore::open(&paths.data).unwrap();
        image_geometry::write(&paths.data, &[ImageGeometry { content_hash: "a".repeat(64), width: 10, height: 20 }]).unwrap();
        drop(store);
        assert!(paths.data.join("persistent/image-geometry.sqlite").exists());
        erase_with(&paths, &mut request(), &|_| Ok(()), files::remove).unwrap();
        assert!(!paths.data.exists());
    }
    #[test]
    fn cleanup_closes_the_lease_owner_before_erasing_and_reopens_the_new_cache() {
        use crate::external_storage::{contract::ErrorKind, job_store::JobCommandState, lease_ledger::LocalLeaseLedger};
        use risunest_external_storage_format::control::{LeaseDocument, LeaseKind};
        let root = tempfile::tempdir().unwrap();
        let mut paths = paths(root.path());
        let cache = root.path().join("cache");
        paths.roots.push(cache.clone());
        let state = JobCommandState::default();
        assert!(state.lease_ledger().is_err());
        state.open_lease_ledger(&cache).unwrap();
        let held = state.lease_ledger().unwrap();
        let document = LeaseDocument::new("writer".into(), "operation".into(), LeaseKind::Work, 0, 1_700_000_000_000).unwrap();
        held.record("repository", "connection", "old", &document).unwrap();
        assert!(matches!(LocalLeaseLedger::open(&cache), Err(error) if error.kind == ErrorKind::RepositoryBusy));
        assert!(state.close_lease_ledger_for_cleanup().is_err());
        let worker = state.track_worker();
        state.begin_cleanup().unwrap();
        assert!(state.close_lease_ledger_for_cleanup().is_err());
        assert!(state.lease_ledger().is_err());
        drop(worker);
        assert!(state.cleanup_drained().unwrap());
        assert!(state.close_lease_ledger_for_cleanup().is_err());
        assert!(matches!(LocalLeaseLedger::open(&cache), Err(error) if error.kind == ErrorKind::RepositoryBusy));
        drop(held);
        state.close_lease_ledger_for_cleanup().unwrap();
        erase_with(&paths, &mut request(), &|_| Ok(()), files::remove).unwrap();
        assert!(!cache.exists());
        state.open_lease_ledger(&cache).unwrap();
        assert!(state.lease_ledger().is_err());
        state.finish_cleanup().unwrap();
        let current = state.lease_ledger().unwrap();
        assert!(!current.abandoned("repository", "connection", "old", &document).unwrap());
        current.record("repository", "connection", "new", &document).unwrap();
        current.record("repository", "connection", "released", &document).unwrap();
        current.released("repository", "connection", "released").unwrap();
        assert!(matches!(LocalLeaseLedger::open(&cache), Err(error) if error.kind == ErrorKind::RepositoryBusy));
        drop(current);
        state.begin_cleanup().unwrap();
        state.close_lease_ledger_for_cleanup().unwrap();
        state.open_lease_ledger(&cache).unwrap();
        state.finish_cleanup().unwrap();
        let reopened = state.lease_ledger().unwrap();
        assert!(reopened.abandoned("repository", "connection", "new", &document).unwrap());
        assert!(!reopened.abandoned("repository", "connection", "old", &document).unwrap());
        assert!(!reopened.abandoned("repository", "connection", "released", &document).unwrap());
        let mut changed = document.clone();
        changed.seq += 1;
        assert!(!reopened.abandoned("repository", "connection", "new", &changed).unwrap());
        assert!(matches!(LocalLeaseLedger::open(&cache), Err(error) if error.kind == ErrorKind::RepositoryBusy));
    }

    #[test]
    fn cold_cleanup_removes_all_owned_roots_but_preserves_exports_and_other_product() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        for path in [&paths.data, &paths.roots[1], &root.path().join("sync")] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("synthetic"), b"fixture").unwrap();
        }
        fs::write(root.path().join("backup.risunest"), b"fixture").unwrap();
        erase(&paths, &mut request()).unwrap();
        erase(&paths, &mut request()).unwrap();
        assert!(!paths.data.exists());
        assert!(!paths.roots[1].exists());
        assert!(root.path().join("backup.risunest").exists());
        assert!(root.path().join("sync/synthetic").exists());
    }
    #[test]
    fn a_root_overlapping_the_installation_directory_refuses_before_any_deletion() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = paths(root.path());
        paths.install_conflict = Some(paths.data.clone());
        for path in [&paths.data, &paths.roots[1]] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("synthetic"), b"fixture").unwrap();
        }
        // The refusal happens in the first statement of erase, so neither the
        // OS secret entries nor the files are reached.
        assert_eq!(erase(&paths, &mut request()), Err("cleanup-root-overlaps-install".to_owned()));
        assert!(paths.data.join("synthetic").exists());
        assert!(paths.roots[1].join("synthetic").exists());
    }

    #[test]
    fn corrupt_request_never_becomes_a_normal_start() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        fs::create_dir_all(&paths.control).unwrap();
        fs::write(paths.request(), b"{}").unwrap();
        assert!(read_request(&paths).is_err());
    }

    #[test]
    fn a_cleanup_failure_keeps_its_cause_in_the_memory_log_only() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        fs::create_dir_all(&paths.control).unwrap();
        fs::write(paths.request(), br#"{"version":"private-journal-value"}"#).unwrap();
        assert_eq!(read_request(&paths).err().as_deref(), Some("cleanup-journal-corrupt"));
        let entry = crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .rev()
            .find(|entry| entry.target == "app-cleanup" && entry.message.starts_with("cleanup-journal-corrupt cause=json failure at line 1"))
            .expect("the cause is logged");
        assert_eq!(entry.level, "error");
        assert!(!entry.message.contains("private-journal-value"), "{}", entry.message);
        assert!(entry.message.contains("app_cleanup"), "{}", entry.message);
        let log = crate::native_log::NativeLogState::initialize(root.path());
        log.record_ring_only("error", "app-cleanup", "synthetic ring-only line");
        assert!(log.tail(None).iter().any(|entry| entry.message == "synthetic ring-only line"));
        assert!(!log.file_path().exists());
    }
    #[cfg(desktop)]
    #[test]
    fn cleanup_excludes_a_live_owner_before_deleting_any_data() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let owner = lock(&paths, Duration::ZERO).unwrap();
        assert!(lock(&paths, Duration::ZERO).is_err());
        drop(owner);
        assert!(lock(&paths, Duration::ZERO).is_ok());
    }

    #[test]
    fn unavailable_credentials_preserve_a_cancellable_journal_and_all_roots() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let mut request = request();
        files::write_json(&paths.request(), &request).unwrap();
        let error = erase_with(&paths, &mut request, &|_| Err("secret-cleanup-unavailable".into()),
            |_| panic!("must not remove roots while credentials remain")).unwrap_err();
        assert_eq!(error, "secret-cleanup-unavailable");
        assert!(!cancellable_request(&paths).unwrap().roots_started);
        let restarted = CleanupState::from_paths(paths.clone()).unwrap();
        assert!(restarted.pending.load(Ordering::Acquire));
        remove_request(&paths).unwrap();
        drop(restarted);
        assert!(!CleanupState::from_paths(paths).unwrap().pending.load(Ordering::Acquire));
    }

    #[test]
    fn roots_phase_is_durable_before_the_first_removal_and_blocks_cancel_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let mut request = request();
        files::write_json(&paths.request(), &request).unwrap();
        let error = erase_with(&paths, &mut request, &|_| Ok(()), |_| {
            assert!(read_request(&paths).unwrap().unwrap().roots_started);
            Err("cleanup-files-busy-or-denied".into())
        }).unwrap_err();
        assert_eq!(error, "cleanup-files-busy-or-denied");
        assert_eq!(cancellable_request(&paths).err().as_deref(), Some("cleanup-cancel-unavailable"));
        let restarted = CleanupState::from_paths(paths).unwrap();
        assert!(restarted.pending.load(Ordering::Acquire));
    }

    #[test]
    fn failed_phase_publication_never_removes_roots() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        fs::create_dir_all(paths.request()).unwrap();
        assert!(erase_with(&paths, &mut request(), &|_| Ok(()),
            |_| panic!("must persist phase before deletion")).is_err());
    }

    #[tokio::test]
    async fn cancellation_retains_the_journal_until_resources_reopen() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let mut request = request();
        files::write_json(&paths.request(), &request).unwrap();
        assert_eq!(cancel_with(&paths, async { Err("cleanup-storage-busy".into()) }).await,
            Err("cleanup-storage-busy".into()));
        assert!(read_request(&paths).unwrap().is_some());
        cancel_with(&paths, async {
            assert!(read_request(&paths).unwrap().is_some());
            Ok(())
        }).await.unwrap();
        assert!(read_request(&paths).unwrap().is_none());
        request.roots_started = true;
        files::write_json(&paths.request(), &request).unwrap();
        assert_eq!(cancel_with(&paths, async { panic!("cannot reopen a partially erased library") }).await,
            Err("cleanup-cancel-unavailable".into()));
    }

    #[test]
    fn redirected_layout_allows_normal_start_but_blocks_cleanup_and_pending_recovery() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("real-profile");
        let alias = root.path().join("redirected-profile");
        fs::create_dir_all(&target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            assert!(std::process::Command::new("cmd").creation_flags(0x08000000).args(["/C", "mklink", "/J"])
                .arg(&alias).arg(&target).output().unwrap().status.success());
        }
        let paths = paths(&alias);
        let state = CleanupState::from_paths(paths.clone()).unwrap();
        assert!(!state.pending.load(Ordering::Acquire));
        assert_eq!(state.available().unwrap_err(), "cleanup-path-redirected");
        assert!(erase(&paths, &mut request()).is_err());
        #[cfg(desktop)]
        assert!(lock(&paths, Duration::ZERO).is_err());
        fs::create_dir_all(target.join("control")).unwrap();
        fs::write(target.join("control/request.json"), b"torn").unwrap();
        let restarted = CleanupState::from_paths(paths).unwrap();
        assert!(restarted.pending.load(Ordering::Acquire));
        assert!(restarted.available().is_err());
    }

    #[cfg(desktop)]
    #[test]
    fn unavailable_roots_keep_a_valid_control_ownership_lock() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = paths(root.path());
        paths.install_conflict = Some(paths.data.clone());
        let state = CleanupState::from_paths(paths.clone()).unwrap();
        assert!(state.available().is_err());
        assert!(lock(&paths, Duration::ZERO).is_err());
        drop(state);
        assert!(lock(&paths, Duration::ZERO).is_ok());
    }

    #[cfg(desktop)]
    mod profiles {
        use super::*;
        use std::{cell::RefCell, path::PathBuf};

        struct Fixture {
            _root: tempfile::TempDir,
            profiles: DesktopProfiles,
            webview_root: PathBuf,
        }

        impl Fixture {
            fn new() -> Self {
                let root = tempfile::tempdir().unwrap();
                // macOS hands out temporary directories behind the /var link,
                // which cleanup rightly refuses.
                let base = fs::canonicalize(root.path()).unwrap();
                let base = if cfg!(target_os = "macos") { base } else { root.path().to_owned() };
                let profiles = DesktopProfiles::synthetic(&base, false);
                Self { _root: root, profiles, webview_root: base.join("webview") }
            }

            fn create(&self, profile: Option<&str>) -> PathBuf {
                let manifest = self.profiles.manifest(name(profile).as_ref()).unwrap();
                for root in manifest.owned_roots() {
                    fs::create_dir_all(&root).unwrap();
                    fs::write(root.join("synthetic"), b"fixture").unwrap();
                }
                fs::create_dir_all(self.webview(profile)).unwrap();
                fs::write(self.webview(profile).join("synthetic"), b"fixture").unwrap();
                manifest.data
            }

            fn webview(&self, profile: Option<&str>) -> PathBuf {
                self.webview_root.join(profile.unwrap_or("default"))
            }

            fn exists(&self, profile: Option<&str>) -> bool {
                let manifest = self.profiles.manifest(name(profile).as_ref()).unwrap();
                manifest.data.join("synthetic").exists()
            }

            fn paths(&self, profile: Option<&str>) -> Paths {
                profile_paths(&self.profiles, name(profile).as_ref()).unwrap()
            }

            fn run(&self, args: &[&str], profile: Option<&str>, secrets: &dyn Fn(&std::path::Path) -> Result<()>,
                relaunched: &RefCell<Vec<Option<String>>>) -> Result<()> {
                let webview = |profile: Option<&ProfileName>| files::remove(&self.webview(profile.map(ProfileName::as_str)));
                self.run_with_webview(args, profile, secrets, &webview, relaunched)
            }

            fn run_with_webview(&self, args: &[&str], profile: Option<&str>, secrets: &dyn Fn(&std::path::Path) -> Result<()>,
                webview: &dyn Fn(Option<&ProfileName>) -> Result<()>, relaunched: &RefCell<Vec<Option<String>>>) -> Result<()> {
                let relaunch = |_: &Request, profile: Option<&ProfileName>| {
                    relaunched.borrow_mut().push(profile.map(|profile| profile.as_str().to_owned()));
                    Ok(())
                };
                let effects = Effects { remove_secrets: secrets, remove_webview: webview, relaunch: &relaunch };
                let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
                cli(&args, &self.profiles, name(profile).as_ref(), &effects)
            }
        }

        fn name(profile: Option<&str>) -> Option<ProfileName> {
            profile.map(|profile| ProfileName::parse(profile).unwrap())
        }

        const REMOVE: [&str; 2] = ["--remove-local-data", "--yes"];

        #[test]
        fn removing_one_profile_keeps_every_other_profile() {
            let fixture = Fixture::new();
            fixture.create(None);
            let work = fixture.create(Some("work"));
            fixture.create(Some("test"));
            let removed = RefCell::new(Vec::new());
            let secrets = |root: &std::path::Path| { removed.borrow_mut().push(root.to_owned()); Ok(()) };
            let relaunched = RefCell::new(Vec::new());
            fixture.run(&REMOVE, Some("work"), &secrets, &relaunched).unwrap();
            assert_eq!(*removed.borrow(), vec![work]);
            for directory in fixture.profiles.directories(&ProfileName::parse("work").unwrap()).unwrap() {
                assert!(!directory.exists(), "{}", directory.display());
            }
            assert!(fixture.exists(None));
            assert!(fixture.exists(Some("test")));
            assert!(!fixture.webview(Some("work")).exists());
            assert!(fixture.webview(None).join("synthetic").exists());
            assert!(fixture.webview(Some("test")).join("synthetic").exists());
            assert_eq!(fixture.profiles.names().unwrap(), vec![ProfileName::parse("test").unwrap()]);
            // A profile that was never created is already removed.
            fixture.run(&REMOVE, Some("absent"), &secrets, &relaunched).unwrap();
            assert!(!fixture.profiles.directories(&ProfileName::parse("absent").unwrap()).unwrap()[0].exists());
            assert!(relaunched.borrow().is_empty());
        }

        #[test]
        fn removing_without_a_profile_removes_every_profile_and_the_namespace() {
            let fixture = Fixture::new();
            let default = fixture.create(None);
            let work = fixture.create(Some("work"));
            let test = fixture.create(Some("test"));
            let removed = RefCell::new(Vec::new());
            let secrets = |root: &std::path::Path| { removed.borrow_mut().push(root.to_owned()); Ok(()) };
            fixture.run(&REMOVE, None, &secrets, &RefCell::new(Vec::new())).unwrap();
            // The profile holding the request goes last.
            assert_eq!(*removed.borrow(), vec![test, work, default]);
            assert!(!fixture.exists(None));
            for profile in [None, Some("work"), Some("test")] {
                assert!(!fixture.webview(profile).exists());
            }
            assert!(!fixture.paths(None).control.exists());
            let bases = fixture.profiles.bases().unwrap();
            assert!(!bases.profiles.exists());
            assert!(bases.caches.is_none_or(|caches| !caches.exists()));
        }

        #[test]
        fn a_failed_default_webview_removal_keeps_the_journal_and_can_be_retried() {
            let fixture = Fixture::new();
            fixture.create(None);
            let failing = |_: Option<&ProfileName>| Err("cleanup-webview-timeout".into());
            assert_eq!(fixture.run_with_webview(&REMOVE, None, &|_| Ok(()), &failing, &RefCell::new(Vec::new())),
                Err("cleanup-webview-timeout".into()));
            assert!(fixture.webview(None).join("synthetic").exists());
            let pending = read_request(&fixture.paths(None)).unwrap().unwrap();
            assert_eq!(pending.mode, Mode::PrepareRemoval);
            assert_eq!(pending.error.as_deref(), Some("cleanup-webview-timeout"));
            fixture.run(&REMOVE, None, &|_| Ok(()), &RefCell::new(Vec::new())).unwrap();
            assert!(!fixture.webview(None).exists());
            assert!(!fixture.paths(None).control.exists());
        }

        #[test]
        fn an_interrupted_removal_of_every_profile_finishes_when_repeated() {
            let fixture = Fixture::new();
            fixture.create(None);
            fixture.create(Some("work"));
            let test = fixture.create(Some("test"));
            let failing = |root: &std::path::Path| if root == test { Err("secret-cleanup-unavailable".into()) } else { Ok(()) };
            assert_eq!(fixture.run(&REMOVE, None, &failing, &RefCell::new(Vec::new())),
                Err("secret-cleanup-unavailable".into()));
            // The failed profile keeps its data behind its own pending journal,
            // so it can never start without its credentials.
            assert!(fixture.exists(Some("test")));
            let pending = read_request(&fixture.paths(Some("test"))).unwrap().unwrap();
            assert_eq!(pending.mode, Mode::RemoveProfile);
            assert_eq!(pending.error.as_deref(), Some("secret-cleanup-unavailable"));
            assert_eq!(read_request(&fixture.paths(None)).unwrap().unwrap().mode, Mode::PrepareRemoval);
            assert!(fixture.exists(None));
            fixture.run(&REMOVE, None, &|_| Ok(()), &RefCell::new(Vec::new())).unwrap();
            for profile in [None, Some("work"), Some("test")] {
                assert!(!fixture.exists(profile));
            }
            assert!(!fixture.profiles.bases().unwrap().profiles.exists());
        }

        #[test]
        fn an_interrupted_single_profile_removal_never_resumes_as_a_wider_one() {
            let fixture = Fixture::new();
            fixture.create(None);
            fixture.create(Some("work"));
            fixture.create(Some("test"));
            assert!(fixture.run(&REMOVE, Some("work"), &|_| Err("secret-cleanup-unavailable".into()),
                &RefCell::new(Vec::new())).is_err());
            // The app resumes the pending journal after clearing its WebView.
            let paths = fixture.paths(Some("work"));
            let mut request = read_request(&paths).unwrap().unwrap();
            assert_eq!(request.mode, Mode::RemoveProfile);
            request.webview_cleared = true;
            files::write_json(&paths.request(), &request).unwrap();
            let relaunched = RefCell::new(Vec::new());
            fixture.run(&["--cleanup-worker", &request.token], Some("work"), &|_| Ok(()), &relaunched).unwrap();
            assert!(!fixture.exists(Some("work")));
            assert!(fixture.exists(None));
            assert!(fixture.exists(Some("test")));
            assert!(relaunched.borrow().is_empty());
        }

        #[test]
        fn the_worker_relaunches_the_profile_it_cleaned() {
            let fixture = Fixture::new();
            fixture.create(None);
            fixture.create(Some("work"));
            let paths = fixture.paths(Some("work"));
            let mut request = new_request(Mode::Reset);
            request.webview_cleared = true;
            files::write_json(&paths.request(), &request).unwrap();
            let relaunched = RefCell::new(Vec::new());
            assert!(fixture.run(&["--cleanup-worker", &request.token], Some("work"),
                &|_| Err("secret-cleanup-unavailable".into()), &relaunched).is_err());
            fixture.run(&["--cleanup-worker", &request.token], Some("work"), &|_| Ok(()), &relaunched).unwrap();
            assert_eq!(*relaunched.borrow(), vec![Some("work".to_owned()), Some("work".to_owned())]);
            // A reset keeps the profile itself, and every other profile.
            assert!(!fixture.exists(Some("work")));
            assert!(fixture.profiles.directories(&ProfileName::parse("work").unwrap()).unwrap()[0].exists());
            assert!(fixture.webview(Some("work")).exists());
            assert!(fixture.exists(None));
        }

        #[test]
        fn prepare_removal_refuses_while_another_profile_runs_and_creates_nothing() {
            let fixture = Fixture::new();
            fixture.create(Some("work"));
            fixture.create(Some("test"));
            let work = ProfileName::parse("work").unwrap();
            refuse_while_other_profiles_run(&fixture.profiles, Some(&work)).unwrap();
            // The default profile was never started, and checking it left no trace.
            assert!(!fixture.paths(None).control.exists());
            let running = lock(&fixture.paths(Some("test")), Duration::ZERO).unwrap();
            assert_eq!(refuse_while_other_profiles_run(&fixture.profiles, Some(&work)),
                Err("cleanup-other-profiles-running".into()));
            assert!(fixture.exists(Some("test")));
            drop(running);
            let default = lock(&fixture.paths(None), Duration::ZERO).unwrap();
            assert_eq!(refuse_while_other_profiles_run(&fixture.profiles, Some(&work)),
                Err("cleanup-other-profiles-running".into()));
            drop(default);
            refuse_while_other_profiles_run(&fixture.profiles, Some(&work)).unwrap();
        }

        #[test]
        fn removal_waits_for_a_closing_profile_to_release_its_lock() {
            let fixture = Fixture::new();
            fixture.create(Some("work"));
            let paths = fixture.paths(Some("work"));
            let held = lock(&paths, Duration::ZERO).unwrap();
            let release = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(300)); drop(held); });
            lock(&paths, Duration::from_secs(10)).unwrap();
            release.join().unwrap();
        }

        /// Replaces a profile's data root with a link to a directory outside
        /// it that holds app entries and, optionally, a file of the user's.
        fn link_data_root(fixture: &Fixture, profile: &str, user_file: bool) -> (PathBuf, PathBuf) {
            let data = fixture.create(Some(profile));
            fs::remove_dir_all(&data).unwrap();
            let target = fixture.webview_root.with_file_name(format!("moved-{profile}"));
            for directory in ["persistent/records", "logs", "assets/objects"] {
                fs::create_dir_all(target.join(directory)).unwrap();
                fs::write(target.join(directory).join("synthetic"), b"fixture").unwrap();
            }
            for file in ["external-connections.sqlite", "external-connections.sqlite-wal", crate::app_paths::DATA_ROOT_LOCK, ".tmpA1b2C3"] {
                fs::write(target.join(file), b"fixture").unwrap();
            }
            if user_file { fs::write(target.join("notes.txt"), b"user").unwrap(); }
            crate::app_paths::tests::link_directory(&target, &data);
            (data, target)
        }

        #[test]
        fn removing_a_linked_data_root_keeps_only_what_the_app_did_not_create() {
            let fixture = Fixture::new();
            let (work, work_target) = link_data_root(&fixture, "work", true);
            let (_, test_target) = link_data_root(&fixture, "test", false);
            let removed = RefCell::new(Vec::new());
            let secrets = |root: &std::path::Path| { removed.borrow_mut().push(root.to_owned()); Ok(()) };
            let relaunched = RefCell::new(Vec::new());
            fixture.run(&REMOVE, Some("work"), &secrets, &relaunched).unwrap();
            fixture.run(&REMOVE, Some("test"), &secrets, &relaunched).unwrap();
            // Secrets are keyed by the link, not by where it leads.
            assert_eq!(removed.borrow()[0], work);
            assert_eq!(fs::read_dir(&work_target).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>(), vec!["notes.txt"]);
            assert!(!test_target.exists());
            for profile in ["work", "test"] {
                for directory in fixture.profiles.directories(&ProfileName::parse(profile).unwrap()).unwrap() {
                    assert!(fs::symlink_metadata(&directory).is_err(), "{}", directory.display());
                }
            }
        }

        #[test]
        fn a_reset_clears_a_linked_data_root_and_keeps_the_link() {
            let fixture = Fixture::new();
            let (data, target) = link_data_root(&fixture, "work", true);
            let paths = fixture.paths(Some("work"));
            erase_with(&paths, &mut request(), &|_| Ok(()), files::remove).unwrap();
            assert_eq!(fs::read_dir(&target).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>(), vec!["notes.txt"]);
            assert!(crate::trust_boundary::is_link_like(&fs::symlink_metadata(&data).unwrap()));
            assert_eq!(fs::canonicalize(&data).unwrap(), fs::canonicalize(&target).unwrap());
        }

        #[test]
        fn a_data_root_link_whose_target_is_gone_is_removed() {
            let fixture = Fixture::new();
            let (data, target) = link_data_root(&fixture, "work", false);
            fs::remove_dir_all(&target).unwrap();
            fixture.run(&REMOVE, Some("work"), &|_| Ok(()), &RefCell::new(Vec::new())).unwrap();
            assert!(fs::symlink_metadata(&data).is_err());
        }

        #[test]
        fn cleanup_refuses_a_data_root_another_profile_is_using() {
            let fixture = Fixture::new();
            let (_, target) = link_data_root(&fixture, "work", false);
            let held = DataRootLock::acquire(&target).unwrap();
            assert_eq!(DataRootLock::acquire(&target).err().unwrap(), "data-root-in-use");
            let error = fixture.run(&REMOVE, Some("work"), &|_| panic!("secrets stay until the data root is free"),
                &RefCell::new(Vec::new())).unwrap_err();
            assert_eq!(error, "cleanup-data-root-in-use");
            assert!(target.join("persistent/records/synthetic").exists());
            // The app releases its hold before it exits for the worker.
            held.release();
            fixture.run(&REMOVE, Some("work"), &|_| Ok(()), &RefCell::new(Vec::new())).unwrap();
            assert!(!target.exists());
        }
    }
}
