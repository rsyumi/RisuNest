use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

#[derive(Debug, PartialEq, Eq)]
enum TunnelError {
    CloudflaredNotInstalled,
    InvalidOriginPort,
    InvalidPersistentConfiguration,
    Launch(String),
    Stop {
        process: Option<String>,
        peer_session: Option<String>,
    },
}

impl fmt::Display for TunnelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CloudflaredNotInstalled => formatter.write_str("cloudflared is not installed"),
            Self::InvalidOriginPort => formatter.write_str("peer origin port must not be zero"),
            Self::InvalidPersistentConfiguration => {
                formatter.write_str("persistent tunnel configuration must not be empty")
            }
            Self::Launch(error) => write!(formatter, "failed to launch cloudflared: {error}"),
            Self::Stop {
                process,
                peer_session,
            } => write!(
                formatter,
                "failed to stop tunnel cleanly (process: {}, peer session: {})",
                process.as_deref().unwrap_or("ok"),
                peer_session.as_deref().unwrap_or("ok")
            ),
        }
    }
}

impl std::error::Error for TunnelError {}

fn cloudflared_executable_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }

    #[cfg(not(unix))]
    true
}

fn discover_in_paths<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<PathBuf, TunnelError> {
    paths
        .into_iter()
        .map(|directory| directory.join(cloudflared_executable_name()))
        .find(|candidate| is_executable(candidate))
        .ok_or(TunnelError::CloudflaredNotInstalled)
}

fn discover_cloudflared() -> Result<PathBuf, TunnelError> {
    let Some(path) = env::var_os("PATH") else {
        return Err(TunnelError::CloudflaredNotInstalled);
    };
    let paths = env::split_paths(&path).collect::<Vec<_>>();
    discover_in_paths(paths.iter().map(PathBuf::as_path))
}

trait CloudflaredDiscovery {
    fn discover(&self) -> Result<PathBuf, TunnelError>;
}

struct PathCloudflaredDiscovery;

impl CloudflaredDiscovery for PathCloudflaredDiscovery {
    fn discover(&self) -> Result<PathBuf, TunnelError> {
        discover_cloudflared()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TunnelPersistence {
    OneShotExperimental,
    PersistentConfiguration,
}

enum TunnelMode {
    Quick,
    RemoteToken(String),
    RemoteTokenFile(PathBuf),
    Named { config: PathBuf, tunnel: String },
}

struct LaunchSpec {
    args: Vec<OsString>,
    origin: String,
    persistence: TunnelPersistence,
}

trait TunnelProcess {
    fn stop(&mut self) -> Result<(), String>;
}

trait TunnelProcessLauncher {
    type Process: TunnelProcess;

    fn launch(&self, executable: &Path, launch: &LaunchSpec) -> Result<Self::Process, TunnelError>;
}

trait PeerSession {
    fn revoke(&mut self) -> Result<(), String>;
}

struct SystemTunnelProcess {
    child: Child,
}

impl TunnelProcess for SystemTunnelProcess {
    fn stop(&mut self) -> Result<(), String> {
        match self.child.try_wait().map_err(|error| error.to_string())? {
            Some(_) => Ok(()),
            None => {
                self.child.kill().map_err(|error| error.to_string())?;
                self.child
                    .wait()
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
        }
    }
}

struct SystemTunnelProcessLauncher;

impl TunnelProcessLauncher for SystemTunnelProcessLauncher {
    type Process = SystemTunnelProcess;

    fn launch(&self, executable: &Path, launch: &LaunchSpec) -> Result<Self::Process, TunnelError> {
        let child = Command::new(executable)
            .args(&launch.args)
            .spawn()
            .map_err(|error| TunnelError::Launch(error.to_string()))?;
        Ok(SystemTunnelProcess { child })
    }
}

struct TunnelAdapter<D, L> {
    discovery: D,
    launcher: L,
}

impl<D, L> TunnelAdapter<D, L> {
    fn new(discovery: D, launcher: L) -> Self {
        Self {
            discovery,
            launcher,
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
        origin_port: u16,
        peer_session: S,
    ) -> Result<RunningTunnel<L::Process, S>, TunnelError>
    where
        S: PeerSession,
    {
        let launch = mode.launch(origin_port)?;
        let executable = self.discovery.discover()?;
        let process = self.launcher.launch(&executable, &launch)?;
        Ok(RunningTunnel {
            process,
            peer_session,
            active: true,
        })
    }
}

impl TunnelAdapter<PathCloudflaredDiscovery, SystemTunnelProcessLauncher> {
    fn system() -> Self {
        Self::new(PathCloudflaredDiscovery, SystemTunnelProcessLauncher)
    }
}

struct RunningTunnel<P, S>
where
    P: TunnelProcess,
    S: PeerSession,
{
    process: P,
    peer_session: S,
    active: bool,
}

impl<P, S> RunningTunnel<P, S>
where
    P: TunnelProcess,
    S: PeerSession,
{
    fn stop(mut self) -> Result<(), TunnelError> {
        self.stop_resources()
    }

    fn stop_resources(&mut self) -> Result<(), TunnelError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;

        let process = self.process.stop().err();
        let peer_session = self.peer_session.revoke().err();
        if process.is_none() && peer_session.is_none() {
            Ok(())
        } else {
            Err(TunnelError::Stop {
                process,
                peer_session,
            })
        }
    }
}

impl<P, S> Drop for RunningTunnel<P, S>
where
    P: TunnelProcess,
    S: PeerSession,
{
    fn drop(&mut self) {
        let _ = self.stop_resources();
    }
}

impl TunnelMode {
    fn launch(&self, origin_port: u16) -> Result<LaunchSpec, TunnelError> {
        if origin_port == 0 {
            return Err(TunnelError::InvalidOriginPort);
        }
        let invalid_persistent_configuration = match self {
            Self::Quick => false,
            Self::RemoteToken(token) => token.trim().is_empty(),
            Self::RemoteTokenFile(path) => path.as_os_str().is_empty(),
            Self::Named { config, tunnel } => {
                config.as_os_str().is_empty() || tunnel.trim().is_empty()
            }
        };
        if invalid_persistent_configuration {
            return Err(TunnelError::InvalidPersistentConfiguration);
        }

        let (args, persistence): (Vec<OsString>, TunnelPersistence) = match self {
            Self::Quick => (
                vec![
                    "tunnel".into(),
                    "--no-autoupdate".into(),
                    "--url".into(),
                    format!("http://127.0.0.1:{origin_port}").into(),
                ],
                TunnelPersistence::OneShotExperimental,
            ),
            Self::RemoteToken(token) => (
                vec![
                    "tunnel".into(),
                    "--no-autoupdate".into(),
                    "run".into(),
                    "--token".into(),
                    token.clone().into(),
                ],
                TunnelPersistence::PersistentConfiguration,
            ),
            Self::RemoteTokenFile(path) => (
                vec![
                    "tunnel".into(),
                    "--no-autoupdate".into(),
                    "run".into(),
                    "--token-file".into(),
                    path.as_os_str().to_owned(),
                ],
                TunnelPersistence::PersistentConfiguration,
            ),
            Self::Named { config, tunnel } => (
                vec![
                    "tunnel".into(),
                    "--no-autoupdate".into(),
                    "--config".into(),
                    config.as_os_str().to_owned(),
                    "run".into(),
                    tunnel.clone().into(),
                ],
                TunnelPersistence::PersistentConfiguration,
            ),
        };

        Ok(LaunchSpec {
            args,
            origin: format!("http://127.0.0.1:{origin_port}"),
            persistence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[derive(Default)]
    struct FakeState {
        launches: Vec<(PathBuf, Vec<OsString>)>,
        process_stops: usize,
        session_revokes: usize,
    }

    struct FakeDiscovery {
        executable: Option<PathBuf>,
    }

    impl CloudflaredDiscovery for FakeDiscovery {
        fn discover(&self) -> Result<PathBuf, TunnelError> {
            self.executable
                .clone()
                .ok_or(TunnelError::CloudflaredNotInstalled)
        }
    }

    struct FakeLauncher {
        state: Arc<Mutex<FakeState>>,
        stop_error: Option<String>,
    }

    impl TunnelProcessLauncher for FakeLauncher {
        type Process = FakeProcess;

        fn launch(
            &self,
            executable: &Path,
            launch: &LaunchSpec,
        ) -> Result<Self::Process, TunnelError> {
            self.state
                .lock()
                .unwrap()
                .launches
                .push((executable.to_owned(), launch.args.clone()));
            Ok(FakeProcess {
                state: Arc::clone(&self.state),
                stop_error: self.stop_error.clone(),
            })
        }
    }

    struct FakeProcess {
        state: Arc<Mutex<FakeState>>,
        stop_error: Option<String>,
    }

    impl TunnelProcess for FakeProcess {
        fn stop(&mut self) -> Result<(), String> {
            self.state.lock().unwrap().process_stops += 1;
            match &self.stop_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    struct FakePeerSession {
        state: Arc<Mutex<FakeState>>,
    }

    impl PeerSession for FakePeerSession {
        fn revoke(&mut self) -> Result<(), String> {
            self.state.lock().unwrap().session_revokes += 1;
            Ok(())
        }
    }

    fn make_executable(path: &Path) {
        fs::write(path, b"test executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[test]
    fn quick_tunnel_is_one_shot_and_uses_only_the_loopback_origin() {
        let launch = TunnelMode::Quick.launch(32145).unwrap();

        assert_eq!(
            launch.args,
            [
                "tunnel",
                "--no-autoupdate",
                "--url",
                "http://127.0.0.1:32145"
            ]
        );
        assert_eq!(launch.persistence, TunnelPersistence::OneShotExperimental);
    }

    #[test]
    fn tunnel_rejects_a_zero_origin_port() {
        assert!(matches!(
            TunnelMode::Quick.launch(0),
            Err(TunnelError::InvalidOriginPort)
        ));
    }

    #[test]
    fn remotely_managed_token_tunnel_is_a_persistent_configuration() {
        let launch = TunnelMode::RemoteToken("tunnel-token".into())
            .launch(32145)
            .unwrap();

        assert_eq!(
            launch.args,
            [
                "tunnel",
                "--no-autoupdate",
                "run",
                "--token",
                "tunnel-token"
            ]
        );
        assert_eq!(
            launch.persistence,
            TunnelPersistence::PersistentConfiguration
        );
        assert_eq!(launch.origin, "http://127.0.0.1:32145");
    }

    #[test]
    fn remotely_managed_token_file_is_a_persistent_configuration() {
        let launch = TunnelMode::RemoteTokenFile(PathBuf::from("configured-token.txt"))
            .launch(32145)
            .unwrap();

        assert_eq!(
            launch.args,
            [
                "tunnel",
                "--no-autoupdate",
                "run",
                "--token-file",
                "configured-token.txt"
            ]
        );
        assert_eq!(
            launch.persistence,
            TunnelPersistence::PersistentConfiguration
        );
        assert_eq!(launch.origin, "http://127.0.0.1:32145");
    }

    #[test]
    fn named_tunnel_uses_an_explicit_persistent_config() {
        let launch = TunnelMode::Named {
            config: PathBuf::from("configured-tunnel.yml"),
            tunnel: "risunest-sync".into(),
        }
        .launch(32145)
        .unwrap();

        assert_eq!(
            launch.args,
            [
                "tunnel",
                "--no-autoupdate",
                "--config",
                "configured-tunnel.yml",
                "run",
                "risunest-sync"
            ]
        );
        assert_eq!(
            launch.persistence,
            TunnelPersistence::PersistentConfiguration
        );
        assert_eq!(launch.origin, "http://127.0.0.1:32145");
    }

    #[test]
    fn persistent_tunnels_reject_empty_configuration() {
        let invalid_modes = [
            TunnelMode::RemoteToken("   ".into()),
            TunnelMode::RemoteTokenFile(PathBuf::new()),
            TunnelMode::Named {
                config: PathBuf::from("configured-tunnel.yml"),
                tunnel: " ".into(),
            },
            TunnelMode::Named {
                config: PathBuf::new(),
                tunnel: "risunest-sync".into(),
            },
        ];

        for mode in invalid_modes {
            assert!(mode.launch(32145).is_err());
        }
    }

    #[test]
    fn discovery_accepts_only_an_existing_cloudflared_executable() {
        let directory = tempdir().unwrap();
        make_executable(&directory.path().join(cloudflared_executable_name()));
        make_executable(&directory.path().join("cloudflared-update.exe"));

        let discovered = discover_in_paths([directory.path()]).unwrap();

        assert_eq!(
            discovered,
            directory.path().join(cloudflared_executable_name())
        );
    }

    #[test]
    fn discovery_does_not_install_or_accept_similarly_named_files() {
        let directory = tempdir().unwrap();
        make_executable(&directory.path().join("cloudflared-installer.exe"));
        make_executable(&directory.path().join("cloudflared-update.exe"));

        assert_eq!(
            discover_in_paths([directory.path()]),
            Err(TunnelError::CloudflaredNotInstalled)
        );
    }

    #[test]
    fn adapter_launches_only_the_discovered_executable() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let adapter = TunnelAdapter::new(
            FakeDiscovery {
                executable: Some(PathBuf::from("C:/Tools/cloudflared.exe")),
            },
            FakeLauncher {
                state: Arc::clone(&state),
                stop_error: None,
            },
        );

        let running = adapter
            .start(
                TunnelMode::Quick,
                32145,
                FakePeerSession {
                    state: Arc::clone(&state),
                },
            )
            .unwrap();

        assert_eq!(
            state.lock().unwrap().launches,
            [(
                PathBuf::from("C:/Tools/cloudflared.exe"),
                vec![
                    "tunnel".into(),
                    "--no-autoupdate".into(),
                    "--url".into(),
                    "http://127.0.0.1:32145".into(),
                ]
            )]
        );
        running.stop().unwrap();
    }

    #[test]
    fn missing_executable_leaves_the_peer_session_unchanged() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let adapter = TunnelAdapter::new(
            FakeDiscovery { executable: None },
            FakeLauncher {
                state: Arc::clone(&state),
                stop_error: None,
            },
        );

        let result = adapter.start(
            TunnelMode::Quick,
            32145,
            FakePeerSession {
                state: Arc::clone(&state),
            },
        );

        assert!(matches!(result, Err(TunnelError::CloudflaredNotInstalled)));
        let state = state.lock().unwrap();
        assert!(state.launches.is_empty());
        assert_eq!(state.session_revokes, 0);
    }

    #[test]
    fn stop_attempts_tunnel_shutdown_and_peer_revoke_together() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let adapter = TunnelAdapter::new(
            FakeDiscovery {
                executable: Some(PathBuf::from("cloudflared.exe")),
            },
            FakeLauncher {
                state: Arc::clone(&state),
                stop_error: Some("process stop failed".into()),
            },
        );
        let running = adapter
            .start(
                TunnelMode::Quick,
                32145,
                FakePeerSession {
                    state: Arc::clone(&state),
                },
            )
            .unwrap();

        let error = running.stop().unwrap_err();

        assert_eq!(
            error,
            TunnelError::Stop {
                process: Some("process stop failed".into()),
                peer_session: None,
            }
        );
        let state = state.lock().unwrap();
        assert_eq!(state.process_stops, 1);
        assert_eq!(state.session_revokes, 1);
    }

    #[test]
    fn dropping_a_running_tunnel_stops_and_revokes_both_resources() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let adapter = TunnelAdapter::new(
            FakeDiscovery {
                executable: Some(PathBuf::from("cloudflared.exe")),
            },
            FakeLauncher {
                state: Arc::clone(&state),
                stop_error: None,
            },
        );

        drop(
            adapter
                .start(
                    TunnelMode::Quick,
                    32145,
                    FakePeerSession {
                        state: Arc::clone(&state),
                    },
                )
                .unwrap(),
        );

        let state = state.lock().unwrap();
        assert_eq!(state.process_stops, 1);
        assert_eq!(state.session_revokes, 1);
    }
}
