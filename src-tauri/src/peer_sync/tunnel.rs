use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, Metadata};
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use url::Url;
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

const OUTPUT_LIMIT: usize = 64 * 1024;
const OUTPUT_CHANNEL_CHUNKS: usize = 16;
const SUPERVISOR_POLL: Duration = Duration::from_millis(25);
const SYSTEM_POLL: Duration = Duration::from_millis(10);
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const DROP_CLEANUP_ATTEMPTS: usize = 3;
const NAMED_ORIGIN_VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
const NAMED_ORIGIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const NAMED_ORIGIN_RETRY: Duration = Duration::from_millis(100);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TunnelError {
    CloudflaredNotInstalled,
    UntrustedCloudflared,
    InvalidOrigin,
    Launch(String),
    Readiness {
        reason: String,
        output: String,
    },
    Stop {
        process: Option<String>,
        peer_session: Option<String>,
    },
    SupervisorUnavailable,
    SupervisorTimeout,
}

impl fmt::Display for TunnelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CloudflaredNotInstalled => formatter.write_str("cloudflared is not installed"),
            Self::UntrustedCloudflared => {
                formatter.write_str("cloudflared is not in a trusted installed location")
            }
            Self::InvalidOrigin => {
                formatter.write_str("tunnel origin must be 127.0.0.1 with a nonzero port")
            }
            Self::Launch(error) => write!(formatter, "failed to launch cloudflared: {error}"),
            Self::Readiness { reason, .. } => {
                write!(formatter, "cloudflared did not become ready: {reason}")
            }
            Self::Stop {
                process,
                peer_session,
            } => write!(
                formatter,
                "failed to stop tunnel cleanly (process: {}, peer session: {})",
                process.as_deref().unwrap_or("ok"),
                peer_session.as_deref().unwrap_or("ok")
            ),
            Self::SupervisorUnavailable => formatter.write_str("tunnel supervisor is unavailable"),
            Self::SupervisorTimeout => formatter.write_str("tunnel supervisor timed out"),
        }
    }
}

impl std::error::Error for TunnelError {}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VerifiedExecutable(PathBuf);

impl VerifiedExecutable {
    fn as_path(&self) -> &Path {
        &self.0
    }
}

fn cloudflared_executable_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

fn is_executable(metadata: &Metadata) -> bool {
    if !metadata.is_file() || is_reparse_point(metadata) {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    true
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|root| root.is_absolute())
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect()
}

fn verify_candidate(candidate: &Path, trusted_roots: &[PathBuf]) -> Option<VerifiedExecutable> {
    if !candidate.is_absolute() {
        return None;
    }
    let metadata = fs::symlink_metadata(candidate).ok()?;
    if !is_executable(&metadata) {
        return None;
    }
    let canonical = fs::canonicalize(candidate).ok()?;
    if !canonical.is_absolute() || !trusted_roots.iter().any(|root| canonical.starts_with(root)) {
        return None;
    }
    Some(VerifiedExecutable(canonical))
}

fn discover_in_paths<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    trusted_roots: &[PathBuf],
) -> Result<VerifiedExecutable, TunnelError> {
    let trusted_roots = canonical_roots(trusted_roots);
    let mut found_untrusted = false;

    for directory in paths {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(cloudflared_executable_name());
        if fs::symlink_metadata(directory)
            .map(|metadata| is_reparse_point(&metadata))
            .unwrap_or(false)
        {
            if candidate.exists() {
                found_untrusted = true;
            }
            continue;
        }
        if !candidate.exists() {
            continue;
        }
        if let Some(executable) = verify_candidate(&candidate, &trusted_roots) {
            return Ok(executable);
        }
        found_untrusted = true;
    }

    if found_untrusted {
        Err(TunnelError::UntrustedCloudflared)
    } else {
        Err(TunnelError::CloudflaredNotInstalled)
    }
}

fn system_trusted_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        [r"C:\Program Files", r"C:\Program Files (x86)"]
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }

    #[cfg(not(windows))]
    {
        ["/usr/bin", "/usr/local/bin"]
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }
}

trait CloudflaredDiscovery {
    fn discover(&self) -> Result<VerifiedExecutable, TunnelError>;
}

struct PathCloudflaredDiscovery {
    trusted_roots: Vec<PathBuf>,
}

impl PathCloudflaredDiscovery {
    fn system() -> Self {
        Self {
            trusted_roots: system_trusted_roots(),
        }
    }
}

impl CloudflaredDiscovery for PathCloudflaredDiscovery {
    fn discover(&self) -> Result<VerifiedExecutable, TunnelError> {
        let Some(path) = env::var_os("PATH") else {
            return Err(TunnelError::CloudflaredNotInstalled);
        };
        let paths = env::split_paths(&path).collect::<Vec<_>>();
        discover_in_paths(paths.iter().map(PathBuf::as_path), &self.trusted_roots)
    }
}

pub(crate) enum TunnelMode {
    Quick,
}

struct LaunchSpec {
    args: Vec<OsString>,
    env_remove: [&'static str; 2],
    token_env: Option<String>,
    // Asserted by the launch-spec tests.
    #[cfg_attr(not(test), allow(dead_code))]
    origin: String,
    readiness: ProcessReadiness,
}

enum ProcessReadiness {
    Quick,
}

impl TunnelMode {
    fn launch(self, origin: SocketAddr) -> Result<LaunchSpec, TunnelError> {
        let SocketAddr::V4(origin) = origin else {
            return Err(TunnelError::InvalidOrigin);
        };
        if origin.ip() != &Ipv4Addr::LOCALHOST || origin.port() == 0 {
            return Err(TunnelError::InvalidOrigin);
        }
        let origin = format!("http://{origin}");

        let (args, token_env, readiness) = match self {
            Self::Quick => (
                vec![
                    "tunnel".into(),
                    "--no-autoupdate".into(),
                    "--url".into(),
                    origin.clone().into(),
                ],
                None,
                ProcessReadiness::Quick,
            ),
        };

        Ok(LaunchSpec {
            args,
            env_remove: ["TUNNEL_TOKEN", "TUNNEL_TOKEN_FILE"],
            token_env,
            origin,
            readiness,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TunnelReady {
    transport_url: Url,
}

pub(crate) trait TunnelProcess: Send + 'static {
    fn wait_ready(
        &mut self,
        timeout: Duration,
        output_limit: usize,
    ) -> Result<TunnelReady, TunnelError>;
    fn poll_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, String>;
    fn stop(&mut self, timeout: Duration) -> Result<(), String>;
}

trait TunnelProcessLauncher {
    type Process: TunnelProcess;

    fn launch(
        &self,
        executable: &VerifiedExecutable,
        launch: LaunchSpec,
    ) -> Result<Self::Process, TunnelError>;
}

pub(crate) trait PeerSession: Send + 'static {
    fn revoke(&mut self) -> Result<(), String>;
    fn verify_named_origin(
        &mut self,
        expected_public_base_url: &Url,
        timeout: Duration,
    ) -> Result<(), String>;
}

struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedOutput {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.limit == 0 {
            return;
        }
        if chunk.len() >= self.limit {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&chunk[chunk.len() - self.limit..]);
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.limit);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
        self.bytes.extend_from_slice(chunk);
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

fn parse_quick_tunnel_url(output: &str) -> Option<Url> {
    let mut offset = 0;
    while let Some(found) = output[offset..].find("https://") {
        let start = offset + found;
        let tail = &output[start..];
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let candidate = tail[..end].trim_end_matches([')', ']', '}', ',', ';', '"', '\'']);
        if let Ok(url) = Url::parse(candidate) {
            let host = url.host_str().unwrap_or_default();
            let prefix = host.strip_suffix(".trycloudflare.com");
            if url.scheme() == "https"
                && prefix.is_some_and(|value| !value.is_empty())
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && matches!(url.path(), "" | "/")
                && url.query().is_none()
                && url.fragment().is_none()
            {
                return Some(url);
            }
        }
        offset = start + "https://".len();
    }
    None
}

fn safe_readiness_error_output(readiness: &ProcessReadiness, output: &BoundedOutput) -> String {
    match readiness {
        ProcessReadiness::Quick => output.text(),
    }
}

pub(crate) struct SystemTunnelProcess {
    child: Child,
    #[cfg(windows)]
    _kill_on_close_job: KillOnCloseJob,
    output_rx: Receiver<Vec<u8>>,
    output: BoundedOutput,
    readiness: ProcessReadiness,
}

#[cfg(windows)]
struct KillOnCloseJob(OwnedHandle);

#[cfg(windows)]
impl KillOnCloseJob {
    fn create() -> std::io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let information_size = u32::try_from(std::mem::size_of_val(&limits))
            .expect("job information size fits in u32");
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                information_size,
            )
        };
        if configured == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    fn assign(&self, child: &Child) -> std::io::Result<()> {
        let assigned =
            unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) };
        if assigned == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl SystemTunnelProcess {
    fn capture_output(&mut self) {
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.output.push(&chunk);
        }
    }

    fn exited_error(&self, status: ExitStatus) -> TunnelError {
        TunnelError::Readiness {
            reason: format!("process exited with {}", exit_label(status)),
            output: self.safe_error_output(),
        }
    }

    fn safe_error_output(&self) -> String {
        safe_readiness_error_output(&self.readiness, &self.output)
    }
}

impl TunnelProcess for SystemTunnelProcess {
    fn wait_ready(
        &mut self,
        timeout: Duration,
        output_limit: usize,
    ) -> Result<TunnelReady, TunnelError> {
        let deadline = Instant::now() + timeout;
        self.output.limit = output_limit;

        loop {
            self.capture_output();
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| TunnelError::Readiness {
                    reason: error.to_string(),
                    output: self.safe_error_output(),
                })?
            {
                self.capture_output();
                return Err(self.exited_error(status));
            }
            match &self.readiness {
                ProcessReadiness::Quick => {
                    if let Some(transport_url) = parse_quick_tunnel_url(&self.output.text()) {
                        return Ok(TunnelReady { transport_url });
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(TunnelError::Readiness {
                    reason: "startup timeout".into(),
                    output: self.safe_error_output(),
                });
            }
            let wait = deadline.saturating_duration_since(now).min(SYSTEM_POLL);
            if let Ok(chunk) = self.output_rx.recv_timeout(wait) {
                self.output.push(&chunk);
            }
        }
    }

    fn poll_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.capture_output();
            if let Some(status) = self.child.try_wait().map_err(|error| error.to_string())? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(SYSTEM_POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn stop(&mut self, timeout: Duration) -> Result<(), String> {
        if self
            .child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }
        if let Err(error) = self.child.kill() {
            if self
                .child
                .try_wait()
                .map_err(|wait| wait.to_string())?
                .is_some()
            {
                return Ok(());
            }
            return Err(error.to_string());
        }

        let deadline = Instant::now() + timeout;
        loop {
            if self
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("process stop timeout".into());
            }
            thread::sleep(SYSTEM_POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
}

impl Drop for SystemTunnelProcess {
    fn drop(&mut self) {
        for _ in 0..DROP_CLEANUP_ATTEMPTS {
            if self.stop(DEFAULT_STOP_TIMEOUT).is_ok() {
                break;
            }
        }
    }
}

fn exit_label(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("code {code}"))
        .unwrap_or_else(|| "no exit code".into())
}

fn output_channel() -> (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) {
    mpsc::sync_channel(OUTPUT_CHANNEL_CHUNKS)
}

fn spawn_reader(mut reader: impl Read + Send + 'static, output_tx: SyncSender<Vec<u8>>) {
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if output_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

struct SystemTunnelProcessLauncher;

impl TunnelProcessLauncher for SystemTunnelProcessLauncher {
    type Process = SystemTunnelProcess;

    fn launch(
        &self,
        executable: &VerifiedExecutable,
        launch: LaunchSpec,
    ) -> Result<Self::Process, TunnelError> {
        let LaunchSpec {
            args,
            env_remove,
            token_env,
            origin: _,
            readiness,
        } = launch;
        let mut command = Command::new(executable.as_path());
        command
            .args(args)
            .env_remove(env_remove[0])
            .env_remove(env_remove[1])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(token) = token_env {
            command.env("TUNNEL_TOKEN", token);
        }
        configure_no_window(&mut command);

        #[cfg(windows)]
        let kill_on_close_job =
            KillOnCloseJob::create().map_err(|error| TunnelError::Launch(error.to_string()))?;

        let mut child = command
            .spawn()
            .map_err(|error| TunnelError::Launch(error.to_string()))?;
        #[cfg(windows)]
        if let Err(error) = kill_on_close_job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TunnelError::Launch(error.to_string()));
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TunnelError::Launch("stdout pipe unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TunnelError::Launch("stderr pipe unavailable".into()))?;
        let (output_tx, output_rx) = output_channel();
        spawn_reader(stdout, output_tx.clone());
        spawn_reader(stderr, output_tx);
        Ok(SystemTunnelProcess {
            child,
            #[cfg(windows)]
            _kill_on_close_job: kill_on_close_job,
            output_rx,
            output: BoundedOutput::new(OUTPUT_LIMIT),
            readiness,
        })
    }
}

#[cfg(windows)]
fn configure_no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_no_window(_command: &mut Command) {}

pub(crate) struct TunnelStartFailure<P: TunnelProcess, S: PeerSession> {
    #[cfg_attr(not(test), allow(dead_code))]
    error: TunnelError,
    process: Option<P>,
    peer_session: Option<S>,
    process_cleanup_error: Option<String>,
}

impl<P: TunnelProcess, S: PeerSession> TunnelStartFailure<P, S> {
    // Asserted by the tunnel seam tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn error_message(&self) -> String {
        self.error.to_string()
    }

    // Recovering the peer session from a failed start is asserted by the tunnel
    // seam tests; production start failures are surfaced, not unwound.
    #[cfg(test)]
    pub(crate) fn into_peer_session(mut self) -> Result<S, Self> {
        if self.process.is_none() {
            Ok(self.peer_session.take().expect("peer session is owned"))
        } else {
            Err(self)
        }
    }
}

fn has_explicit_authority_port(value: &str) -> bool {
    let Some((_, remainder)) = value.split_once("://") else {
        return false;
    };
    remainder
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .contains(':')
}

fn has_bare_authority_path(value: &str) -> bool {
    let Some((_, remainder)) = value.split_once("://") else {
        return false;
    };
    let Some(path_start) = remainder.find('/') else {
        return true;
    };
    let path = &remainder[path_start..];
    let path_end = path.find(['?', '#']).unwrap_or(path.len());
    &path[..path_end] == "/"
}

impl<P: TunnelProcess, S: PeerSession> Drop for TunnelStartFailure<P, S> {
    fn drop(&mut self) {
        let mut stopped = false;
        if let Some(process) = &mut self.process {
            for _ in 0..DROP_CLEANUP_ATTEMPTS {
                if process.stop(DEFAULT_STOP_TIMEOUT).is_ok() {
                    stopped = true;
                    break;
                }
            }
        }
        if stopped {
            self.process = None;
        }

        let mut revoked = false;
        if let Some(peer_session) = &mut self.peer_session {
            for _ in 0..DROP_CLEANUP_ATTEMPTS {
                if peer_session.revoke().is_ok() {
                    revoked = true;
                    break;
                }
            }
        }
        if revoked {
            self.peer_session = None;
        }
    }
}

impl<P, S> TunnelStartFailure<P, S>
where
    P: TunnelProcess,
    S: PeerSession,
{
    fn retry_process_stop(&mut self, timeout: Duration) -> Result<(), String> {
        let Some(process) = &mut self.process else {
            return Ok(());
        };
        process.stop(timeout)?;
        self.process = None;
        self.process_cleanup_error = None;
        Ok(())
    }

    pub(crate) fn retry_cleanup(&mut self) -> Result<(), TunnelError> {
        let process_error = self.retry_process_stop(DEFAULT_STOP_TIMEOUT).err();
        let peer_session_error = if let Some(peer_session) = self.peer_session.as_mut() {
            match peer_session.revoke() {
                Ok(()) => {
                    self.peer_session = None;
                    None
                }
                Err(error) => Some(error),
            }
        } else {
            None
        };
        if process_error.is_none() && peer_session_error.is_none() {
            Ok(())
        } else {
            Err(TunnelError::Stop {
                process: process_error,
                peer_session: peer_session_error,
            })
        }
    }
}

struct TunnelAdapter<D, L> {
    discovery: D,
    launcher: L,
    startup_timeout: Duration,
    startup_cleanup_timeout: Duration,
}

impl<D, L> TunnelAdapter<D, L> {
    fn new(discovery: D, launcher: L) -> Self {
        Self {
            discovery,
            launcher,
            startup_timeout: Duration::from_secs(15),
            startup_cleanup_timeout: DEFAULT_STOP_TIMEOUT,
        }
    }
}

impl<D, L> TunnelAdapter<D, L>
where
    D: CloudflaredDiscovery,
    L: TunnelProcessLauncher,
{
    fn start<S>(
        &self,
        mode: TunnelMode,
        origin: SocketAddr,
        peer_session: S,
    ) -> Result<RunningTunnel, TunnelStartFailure<L::Process, S>>
    where
        S: PeerSession,
    {
        let launch = match mode.launch(origin) {
            Ok(launch) => launch,
            Err(error) => {
                return Err(TunnelStartFailure {
                    error,
                    process: None,
                    peer_session: Some(peer_session),
                    process_cleanup_error: None,
                });
            }
        };
        let executable = match self.discovery.discover() {
            Ok(executable) => executable,
            Err(error) => {
                return Err(TunnelStartFailure {
                    error,
                    process: None,
                    peer_session: Some(peer_session),
                    process_cleanup_error: None,
                });
            }
        };
        let mut process = match self.launcher.launch(&executable, launch) {
            Ok(process) => process,
            Err(error) => {
                return Err(TunnelStartFailure {
                    error,
                    process: None,
                    peer_session: Some(peer_session),
                    process_cleanup_error: None,
                });
            }
        };
        let ready = match process.wait_ready(self.startup_timeout, OUTPUT_LIMIT) {
            Ok(ready) => ready,
            Err(error) => {
                let process_cleanup_error = process.stop(self.startup_cleanup_timeout).err();
                return Err(TunnelStartFailure {
                    error,
                    process: process_cleanup_error.as_ref().map(|_| process),
                    peer_session: Some(peer_session),
                    process_cleanup_error,
                });
            }
        };

        Ok(RunningTunnel::spawn(process, peer_session, ready))
    }
}

impl TunnelAdapter<PathCloudflaredDiscovery, SystemTunnelProcessLauncher> {
    fn system() -> Self {
        Self::new(
            PathCloudflaredDiscovery::system(),
            SystemTunnelProcessLauncher,
        )
    }
}

fn verify_named_route(
    expected_public_base_url: &Url,
    probe: &super::lan::TunnelOriginProbe,
    timeout: Duration,
) -> Result<(), String> {
    let probe_url = expected_public_base_url
        .join(probe.path.trim_start_matches('/'))
        .map_err(|_| "named tunnel origin verification failed".to_owned())?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(NAMED_ORIGIN_REQUEST_TIMEOUT)
        .timeout(NAMED_ORIGIN_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| "named tunnel origin verification failed".to_owned())?;
    let deadline = Instant::now() + timeout;

    loop {
        if let Ok(response) = client
            .get(probe_url.clone())
            .timeout(NAMED_ORIGIN_REQUEST_TIMEOUT)
            .send()
        {
            if response.status() == reqwest::StatusCode::OK {
                let mut body = Vec::new();
                if response
                    .take(probe.expected_body.len() as u64 + 1)
                    .read_to_end(&mut body)
                    .is_ok()
                    && body.as_slice() == probe.expected_body
                {
                    return Ok(());
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("named tunnel origin verification failed".to_owned());
        }
        thread::sleep(NAMED_ORIGIN_RETRY.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub(crate) fn validate_public_base_url(value: &str) -> Option<Url> {
    if has_explicit_authority_port(value) || !has_bare_authority_path(value) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    let is_public_domain = matches!(
        url.host(),
        Some(url::Host::Domain(host)) if host.contains('.') && !host.eq_ignore_ascii_case("localhost")
    );
    if url.scheme() != "https"
        || !is_public_domain
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url)
}

pub(crate) fn verify_public_origin<S>(
    peer_session: &mut S,
    expected_public_base_url: &Url,
) -> Result<(), String>
where
    S: PeerSession,
{
    peer_session.verify_named_origin(expected_public_base_url, NAMED_ORIGIN_VERIFY_TIMEOUT)
}

impl PeerSession for super::LanCloneHost {
    fn revoke(&mut self) -> Result<(), String> {
        self.stop()
            .map_err(|_| "peer session cleanup failed".to_owned())
    }

    fn verify_named_origin(
        &mut self,
        expected_public_base_url: &Url,
        timeout: Duration,
    ) -> Result<(), String> {
        let probe = self
            .issue_tunnel_probe()
            .map_err(|_| "named tunnel origin verification failed".to_owned())?;
        let result = verify_named_route(expected_public_base_url, &probe, timeout);
        self.clear_tunnel_probe();
        result
    }
}

impl PeerSession for super::shared_session::SharedSessionHost {
    fn revoke(&mut self) -> Result<(), String> {
        self.stop()
            .map_err(|_| "shared peer session cleanup failed".to_owned())
    }

    fn verify_named_origin(
        &mut self,
        expected_public_base_url: &Url,
        timeout: Duration,
    ) -> Result<(), String> {
        let probe = self
            .issue_tunnel_probe()
            .map_err(|_| "shared origin verification failed".to_owned())?;
        let result = verify_named_route(expected_public_base_url, &probe, timeout);
        self.clear_tunnel_probe();
        result
    }
}

pub(crate) fn start_quick_desktop_tunnel(
    peer_session: super::LanCloneHost,
) -> Result<RunningTunnel, TunnelStartFailure<SystemTunnelProcess, super::LanCloneHost>> {
    let Some(origin) = peer_session.address() else {
        return Err(TunnelStartFailure {
            error: TunnelError::InvalidOrigin,
            process: None,
            peer_session: Some(peer_session),
            process_cleanup_error: None,
        });
    };
    start_quick_desktop_tunnel_session(peer_session, origin)
}

pub(crate) fn start_quick_desktop_tunnel_session<S>(
    peer_session: S,
    origin: SocketAddr,
) -> Result<RunningTunnel, TunnelStartFailure<SystemTunnelProcess, S>>
where
    S: PeerSession,
{
    TunnelAdapter::system().start(TunnelMode::Quick, origin, peer_session)
}

enum SupervisorCommand {
    Stop {
        timeout: Duration,
        response: Sender<Result<(), TunnelError>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TerminalReason {
    ProcessExited(Option<i32>),
    ProcessExitedCleanupPending(Option<i32>),
    ProcessMonitorFailed {
        error: String,
        process_cleanup_error: Option<String>,
        peer_session_cleanup_error: Option<String>,
    },
    OwnerDisconnected {
        process_cleanup_error: Option<String>,
        peer_session_cleanup_error: Option<String>,
    },
    Stopped,
}

impl TerminalReason {
    fn cleanup_complete(&self) -> bool {
        match self {
            Self::ProcessMonitorFailed {
                process_cleanup_error,
                peer_session_cleanup_error,
                ..
            }
            | Self::OwnerDisconnected {
                process_cleanup_error,
                peer_session_cleanup_error,
            } => process_cleanup_error.is_none() && peer_session_cleanup_error.is_none(),
            Self::ProcessExitedCleanupPending(_) => false,
            Self::ProcessExited(_) | Self::Stopped => true,
        }
    }
}

pub(crate) struct RunningTunnel {
    ready: TunnelReady,
    command_tx: Option<Sender<SupervisorCommand>>,
    terminal_rx: Receiver<TerminalReason>,
    thread: Option<JoinHandle<()>>,
    lifecycle: RunningTunnelLifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunningTunnelLifecycle {
    Running,
    CleanupPending,
    Stopped,
}

impl RunningTunnel {
    fn spawn<P, S>(process: P, peer_session: S, ready: TunnelReady) -> Self
    where
        P: TunnelProcess,
        S: PeerSession,
    {
        let (command_tx, command_rx) = mpsc::channel();
        let (terminal_tx, terminal_rx) = mpsc::channel();
        let thread =
            thread::spawn(move || supervise(process, peer_session, command_rx, terminal_tx));
        Self {
            ready,
            command_tx: Some(command_tx),
            terminal_rx,
            thread: Some(thread),
            lifecycle: RunningTunnelLifecycle::Running,
        }
    }

    pub(crate) fn transport_url(&self) -> &Url {
        &self.ready.transport_url
    }

    pub(crate) fn stop(&mut self, timeout: Duration) -> Result<(), TunnelError> {
        if self.poll_lifecycle()? == RunningTunnelLifecycle::Stopped {
            return Ok(());
        }
        let (response, result) = mpsc::channel();
        let sent = self
            .command_tx
            .as_ref()
            .ok_or(TunnelError::SupervisorUnavailable)?
            .send(SupervisorCommand::Stop { timeout, response });
        if sent.is_err() {
            return self.finish_natural_stop_or_unavailable();
        }
        match result.recv_timeout(timeout.saturating_add(DEFAULT_STOP_TIMEOUT)) {
            Ok(Ok(())) => {
                self.lifecycle = RunningTunnelLifecycle::Stopped;
                self.join_supervisor();
                Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(TunnelError::SupervisorTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => self.finish_natural_stop_or_unavailable(),
        }
    }

    fn finish_natural_stop_or_unavailable(&mut self) -> Result<(), TunnelError> {
        match self.poll_lifecycle() {
            Ok(RunningTunnelLifecycle::Stopped) => Ok(()),
            _ => Err(TunnelError::SupervisorUnavailable),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn wait_terminal(&mut self, timeout: Duration) -> Result<TerminalReason, TunnelError> {
        let reason = self
            .terminal_rx
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => TunnelError::SupervisorTimeout,
                mpsc::RecvTimeoutError::Disconnected => TunnelError::SupervisorUnavailable,
            })?;
        self.record_terminal(&reason);
        Ok(reason)
    }

    pub(crate) fn poll_lifecycle(&mut self) -> Result<RunningTunnelLifecycle, TunnelError> {
        if self.lifecycle == RunningTunnelLifecycle::Stopped {
            return Ok(RunningTunnelLifecycle::Stopped);
        }
        match self.terminal_rx.try_recv() {
            Ok(reason) => self.record_terminal(&reason),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) if self.thread.is_none() => {
                self.lifecycle = RunningTunnelLifecycle::Stopped;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(TunnelError::SupervisorUnavailable);
            }
        }
        Ok(self.lifecycle)
    }

    fn record_terminal(&mut self, reason: &TerminalReason) {
        if self.lifecycle == RunningTunnelLifecycle::Stopped {
            return;
        }
        self.lifecycle = if reason.cleanup_complete() {
            RunningTunnelLifecycle::Stopped
        } else {
            RunningTunnelLifecycle::CleanupPending
        };
        if self.lifecycle == RunningTunnelLifecycle::Stopped {
            self.join_supervisor();
        }
    }

    fn join_supervisor(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RunningTunnel {
    fn drop(&mut self) {
        for _ in 0..DROP_CLEANUP_ATTEMPTS {
            if self.thread.is_none() || self.stop(DEFAULT_STOP_TIMEOUT).is_ok() {
                break;
            }
            thread::sleep(SUPERVISOR_POLL);
        }
        self.command_tx.take();
        self.join_supervisor();
    }
}

enum SupervisorCommandState {
    Command(SupervisorCommand),
    Idle,
    Disconnected,
}

fn next_supervisor_command(
    command_rx: &Receiver<SupervisorCommand>,
    wait_for_command: bool,
) -> SupervisorCommandState {
    if wait_for_command {
        return match command_rx.recv() {
            Ok(command) => SupervisorCommandState::Command(command),
            Err(_) => SupervisorCommandState::Disconnected,
        };
    }

    match command_rx.try_recv() {
        Ok(command) => SupervisorCommandState::Command(command),
        Err(mpsc::TryRecvError::Empty) => SupervisorCommandState::Idle,
        Err(mpsc::TryRecvError::Disconnected) => SupervisorCommandState::Disconnected,
    }
}

fn cleanup_resources<P, S>(
    process: &mut P,
    peer_session: &mut S,
    process_done: &mut bool,
    peer_done: &mut bool,
    timeout: Duration,
) -> (Option<String>, Option<String>)
where
    P: TunnelProcess,
    S: PeerSession,
{
    let process_error = if *process_done {
        None
    } else {
        match process.stop(timeout) {
            Ok(()) => {
                *process_done = true;
                None
            }
            Err(error) => Some(error),
        }
    };
    let peer_error = if *peer_done {
        None
    } else {
        match peer_session.revoke() {
            Ok(()) => {
                *peer_done = true;
                None
            }
            Err(error) => Some(error),
        }
    };
    (process_error, peer_error)
}

fn supervise<P, S>(
    mut process: P,
    mut peer_session: S,
    command_rx: Receiver<SupervisorCommand>,
    terminal_tx: Sender<TerminalReason>,
) where
    P: TunnelProcess,
    S: PeerSession,
{
    let mut process_done = false;
    let mut peer_done = false;
    let mut monitor_failed = false;

    loop {
        match next_supervisor_command(&command_rx, process_done || monitor_failed) {
            SupervisorCommandState::Command(SupervisorCommand::Stop { timeout, response }) => {
                let (process_error, peer_error) = cleanup_resources(
                    &mut process,
                    &mut peer_session,
                    &mut process_done,
                    &mut peer_done,
                    timeout,
                );
                if process_done && peer_done {
                    let _ = response.send(Ok(()));
                    let _ = terminal_tx.send(TerminalReason::Stopped);
                    return;
                }
                let _ = response.send(Err(TunnelError::Stop {
                    process: process_error,
                    peer_session: peer_error,
                }));
            }
            SupervisorCommandState::Disconnected => {
                let (process_cleanup_error, peer_session_cleanup_error) = cleanup_resources(
                    &mut process,
                    &mut peer_session,
                    &mut process_done,
                    &mut peer_done,
                    DEFAULT_STOP_TIMEOUT,
                );
                let _ = terminal_tx.send(TerminalReason::OwnerDisconnected {
                    process_cleanup_error,
                    peer_session_cleanup_error,
                });
                return;
            }
            SupervisorCommandState::Idle => match process.poll_exit(SUPERVISOR_POLL) {
                Ok(Some(status)) => {
                    process_done = true;
                    if !peer_done && peer_session.revoke().is_ok() {
                        peer_done = true;
                    }
                    if peer_done {
                        let _ = terminal_tx.send(TerminalReason::ProcessExited(status.code()));
                        return;
                    }
                    let _ = terminal_tx
                        .send(TerminalReason::ProcessExitedCleanupPending(status.code()));
                }
                Ok(None) => {}
                Err(error) => {
                    monitor_failed = true;
                    let (process_cleanup_error, peer_session_cleanup_error) = cleanup_resources(
                        &mut process,
                        &mut peer_session,
                        &mut process_done,
                        &mut peer_done,
                        DEFAULT_STOP_TIMEOUT,
                    );
                    let cleanup_complete =
                        process_cleanup_error.is_none() && peer_session_cleanup_error.is_none();
                    let _ = terminal_tx.send(TerminalReason::ProcessMonitorFailed {
                        error,
                        process_cleanup_error,
                        peer_session_cleanup_error,
                    });
                    if cleanup_complete {
                        return;
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
