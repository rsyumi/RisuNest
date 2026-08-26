use super::*;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

#[derive(Default)]
struct FakeState {
    launches: Vec<(PathBuf, Vec<OsString>, Vec<&'static str>)>,
    readiness_calls: Vec<(Duration, usize)>,
    stop_timeouts: Vec<Duration>,
    session_revokes: usize,
    process_drops: usize,
    peer_session_drops: usize,
}

struct FakeDiscovery {
    executable: Option<VerifiedExecutable>,
    error: Option<TunnelError>,
}

impl CloudflaredDiscovery for FakeDiscovery {
    fn discover(&self) -> Result<VerifiedExecutable, TunnelError> {
        self.executable.clone().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or(TunnelError::CloudflaredNotInstalled)
        })
    }
}

struct FakeLauncher {
    state: Arc<Mutex<FakeState>>,
    launch_error: Option<String>,
    process: Mutex<Option<FakeProcess>>,
}

impl TunnelProcessLauncher for FakeLauncher {
    type Process = FakeProcess;

    fn launch(
        &self,
        executable: &VerifiedExecutable,
        launch: &LaunchSpec,
    ) -> Result<Self::Process, TunnelError> {
        self.state.lock().unwrap().launches.push((
            executable.as_path().to_owned(),
            launch.args.clone(),
            launch.env_remove.to_vec(),
        ));
        if let Some(error) = &self.launch_error {
            return Err(TunnelError::Launch(error.clone()));
        }
        Ok(self.process.lock().unwrap().take().unwrap())
    }
}

struct FakeProcess {
    state: Arc<Mutex<FakeState>>,
    readiness: Option<Result<TunnelReady, TunnelError>>,
    polls: VecDeque<Result<Option<i32>, String>>,
    stop_results: VecDeque<Result<(), String>>,
}

impl TunnelProcess for FakeProcess {
    fn wait_ready(
        &mut self,
        timeout: Duration,
        output_limit: usize,
    ) -> Result<TunnelReady, TunnelError> {
        self.state
            .lock()
            .unwrap()
            .readiness_calls
            .push((timeout, output_limit));
        self.readiness.take().unwrap()
    }

    fn poll_exit(&mut self, _timeout: Duration) -> Result<Option<ExitStatus>, String> {
        self.polls
            .pop_front()
            .unwrap_or(Ok(None))
            .map(|code| code.map(exit_status))
    }

    fn stop(&mut self, timeout: Duration) -> Result<(), String> {
        self.state.lock().unwrap().stop_timeouts.push(timeout);
        self.stop_results.pop_front().unwrap_or(Ok(()))
    }
}

impl Drop for FakeProcess {
    fn drop(&mut self) {
        self.state.lock().unwrap().process_drops += 1;
    }
}

struct FakePeerSession {
    id: u64,
    state: Arc<Mutex<FakeState>>,
    revoke_results: VecDeque<Result<(), String>>,
}

impl PeerSession for FakePeerSession {
    fn revoke(&mut self) -> Result<(), String> {
        self.state.lock().unwrap().session_revokes += 1;
        self.revoke_results.pop_front().unwrap_or(Ok(()))
    }
}

impl Drop for FakePeerSession {
    fn drop(&mut self) {
        self.state.lock().unwrap().peer_session_drops += 1;
    }
}

fn exit_status(code: i32) -> ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatusExt::from_raw(code as u32)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatusExt::from_raw(code << 8)
    }
}

fn ready() -> TunnelReady {
    TunnelReady {
        transport_url: Url::parse("https://example-id.trycloudflare.com").unwrap(),
    }
}

fn fake_process(
    state: &Arc<Mutex<FakeState>>,
    readiness: Result<TunnelReady, TunnelError>,
) -> FakeProcess {
    FakeProcess {
        state: Arc::clone(state),
        readiness: Some(readiness),
        polls: VecDeque::from([Ok(None)]),
        stop_results: VecDeque::new(),
    }
}

fn adapter(
    state: &Arc<Mutex<FakeState>>,
    process: FakeProcess,
) -> TunnelAdapter<FakeDiscovery, FakeLauncher> {
    TunnelAdapter::new(
        FakeDiscovery {
            executable: Some(VerifiedExecutable(PathBuf::from(
                "C:/Program Files/cloudflared/cloudflared.exe",
            ))),
            error: None,
        },
        FakeLauncher {
            state: Arc::clone(state),
            launch_error: None,
            process: Mutex::new(Some(process)),
        },
    )
}

fn peer(state: &Arc<Mutex<FakeState>>, id: u64) -> FakePeerSession {
    FakePeerSession {
        id,
        state: Arc::clone(state),
        revoke_results: VecDeque::new(),
    }
}

fn expect_failure<P: TunnelProcess, S>(
    result: Result<RunningTunnel, TunnelStartFailure<P, S>>,
) -> TunnelStartFailure<P, S> {
    match result {
        Ok(_) => panic!("expected tunnel start failure"),
        Err(failure) => failure,
    }
}

fn expect_running<P: TunnelProcess, S>(
    result: Result<RunningTunnel, TunnelStartFailure<P, S>>,
) -> RunningTunnel {
    match result {
        Ok(running) => running,
        Err(_) => panic!("expected running tunnel"),
    }
}

fn expect_peer<P: TunnelProcess, S>(failure: TunnelStartFailure<P, S>) -> S {
    match failure.into_peer_session() {
        Ok(peer) => peer,
        Err(_) => panic!("expected peer session ownership"),
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
fn quick_tunnel_has_only_loopback_transport_and_no_token_sources() {
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
    assert_eq!(launch.origin, "http://127.0.0.1:32145");
    assert_eq!(launch.env_remove, ["TUNNEL_TOKEN", "TUNNEL_TOKEN_FILE"]);
    assert!(!launch.args.iter().any(|arg| arg == "--token"));
}

#[test]
fn quick_tunnel_rejects_zero_origin_port() {
    assert!(matches!(
        TunnelMode::Quick.launch(0),
        Err(TunnelError::InvalidOriginPort)
    ));
}

#[test]
fn quick_url_parser_accepts_only_bare_trycloudflare_https_urls() {
    assert_eq!(
        parse_quick_tunnel_url("INF Visit https://valid-id.trycloudflare.com now")
            .unwrap()
            .as_str(),
        "https://valid-id.trycloudflare.com/"
    );
    for invalid in [
        "http://valid-id.trycloudflare.com",
        "https://trycloudflare.com",
        "https://valid-id.trycloudflare.com.evil.test",
        "https://user@valid-id.trycloudflare.com",
        "https://valid-id.trycloudflare.com/path",
        "https://valid-id.trycloudflare.com?token=secret",
    ] {
        assert_eq!(parse_quick_tunnel_url(invalid), None, "{invalid}");
    }
}

#[test]
fn readiness_output_is_bounded_to_the_configured_tail() {
    let mut output = BoundedOutput::new(8);
    output.push(b"012345");
    output.push(b"6789abcdef");

    assert_eq!(output.text(), "89abcdef");
}

#[test]
fn process_output_channel_has_a_fixed_capacity() {
    let (sender, _receiver) = output_channel();

    for _ in 0..OUTPUT_CHANNEL_CHUNKS {
        sender.try_send(vec![0]).unwrap();
    }
    assert!(matches!(
        sender.try_send(vec![0]),
        Err(mpsc::TrySendError::Full(_))
    ));
}

#[test]
fn discovery_accepts_only_canonical_executable_inside_a_trusted_root() {
    let root = tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join(cloudflared_executable_name());
    make_executable(&executable);

    let discovered = discover_in_paths([bin.as_path()], &[root.path().to_owned()]).unwrap();

    assert_eq!(discovered.as_path(), fs::canonicalize(executable).unwrap());
    assert!(discovered.as_path().is_absolute());
}

#[test]
fn discovery_rejects_relative_and_untrusted_path_entries() {
    let root = tempdir().unwrap();
    let untrusted = tempdir().unwrap();
    make_executable(&untrusted.path().join(cloudflared_executable_name()));

    assert_eq!(
        discover_in_paths([Path::new("relative")], &[root.path().to_owned()]),
        Err(TunnelError::CloudflaredNotInstalled)
    );
    assert_eq!(
        discover_in_paths([untrusted.path()], &[root.path().to_owned()]),
        Err(TunnelError::UntrustedCloudflared)
    );
}

#[cfg(unix)]
#[test]
fn discovery_rejects_symlinked_executable_targets() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let real = root.path().join("real-cloudflared");
    make_executable(&real);
    let link = root.path().join(cloudflared_executable_name());
    symlink(&real, &link).unwrap();

    assert_eq!(
        discover_in_paths([root.path()], &[root.path().to_owned()]),
        Err(TunnelError::UntrustedCloudflared)
    );
}

#[cfg(unix)]
#[test]
fn discovery_rejects_symlinked_path_directories() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let real_bin = root.path().join("real-bin");
    fs::create_dir(&real_bin).unwrap();
    make_executable(&real_bin.join(cloudflared_executable_name()));
    let linked_bin = root.path().join("linked-bin");
    symlink(&real_bin, &linked_bin).unwrap();

    assert_eq!(
        discover_in_paths([linked_bin.as_path()], &[root.path().to_owned()]),
        Err(TunnelError::UntrustedCloudflared)
    );
}

#[cfg(windows)]
#[test]
fn discovery_rejects_reparse_point_path_directories() {
    use std::os::windows::fs::symlink_dir;

    let root = tempdir().unwrap();
    let real_bin = root.path().join("real-bin");
    fs::create_dir(&real_bin).unwrap();
    make_executable(&real_bin.join(cloudflared_executable_name()));
    let linked_bin = root.path().join("linked-bin");
    if let Err(error) = symlink_dir(&real_bin, &linked_bin) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("failed to create reparse fixture: {error}");
    }

    assert_eq!(
        discover_in_paths([linked_bin.as_path()], &[root.path().to_owned()]),
        Err(TunnelError::UntrustedCloudflared)
    );
}

#[test]
fn discovery_failure_returns_peer_session_for_lan_fallback() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let adapter = TunnelAdapter::new(
        FakeDiscovery {
            executable: None,
            error: Some(TunnelError::CloudflaredNotInstalled),
        },
        FakeLauncher {
            state: Arc::clone(&state),
            launch_error: None,
            process: Mutex::new(None),
        },
    );

    let failure = expect_failure(adapter.start(TunnelMode::Quick, 32145, peer(&state, 41)));
    let returned = expect_peer(failure);

    assert_eq!(returned.id, 41);
    assert_eq!(state.lock().unwrap().session_revokes, 0);
}

#[test]
fn launch_failure_returns_peer_session_without_exposing_a_token() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let adapter = TunnelAdapter::new(
        FakeDiscovery {
            executable: Some(VerifiedExecutable(PathBuf::from(
                "C:/Program Files/cloudflared/cloudflared.exe",
            ))),
            error: None,
        },
        FakeLauncher {
            state: Arc::clone(&state),
            launch_error: Some("launch failed".into()),
            process: Mutex::new(None),
        },
    );

    let failure = expect_failure(adapter.start(TunnelMode::Quick, 32145, peer(&state, 42)));
    let returned = expect_peer(failure);

    assert_eq!(returned.id, 42);
    let launches = &state.lock().unwrap().launches;
    assert!(!launches[0].1.iter().any(|arg| arg == "--token"));
}

#[test]
fn readiness_failure_stops_process_then_returns_peer_session() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let process = fake_process(
        &state,
        Err(TunnelError::Readiness {
            reason: "startup timeout".into(),
            output: "bounded output".into(),
        }),
    );
    let adapter = adapter(&state, process);

    let failure = expect_failure(adapter.start(TunnelMode::Quick, 32145, peer(&state, 43)));
    let returned = expect_peer(failure);

    assert_eq!(returned.id, 43);
    let state = state.lock().unwrap();
    assert_eq!(
        state.readiness_calls,
        [(Duration::from_secs(15), OUTPUT_LIMIT)]
    );
    assert_eq!(state.stop_timeouts, [DEFAULT_STOP_TIMEOUT]);
    assert_eq!(state.session_revokes, 0);
}

#[test]
fn failed_start_cleanup_preserves_process_for_retry_before_returning_peer() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(
        &state,
        Err(TunnelError::Readiness {
            reason: "early exit".into(),
            output: "bounded output".into(),
        }),
    );
    process.stop_results = VecDeque::from([Err("stop timeout".into()), Ok(())]);
    let adapter = adapter(&state, process);

    let mut failure = expect_failure(adapter.start(TunnelMode::Quick, 32145, peer(&state, 44)));

    assert_eq!(
        failure.process_cleanup_error.as_deref(),
        Some("stop timeout")
    );
    assert!(failure.process.is_some());
    failure.retry_process_stop(DEFAULT_STOP_TIMEOUT).unwrap();
    assert_eq!(expect_peer(failure).id, 44);
}

#[test]
fn dropping_failed_start_retries_child_cleanup_automatically() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(
        &state,
        Err(TunnelError::Readiness {
            reason: "early exit".into(),
            output: "bounded output".into(),
        }),
    );
    process.stop_results = VecDeque::from([
        Err("startup stop failed".into()),
        Err("first drop retry failed".into()),
        Ok(()),
    ]);
    let adapter = adapter(&state, process);

    drop(expect_failure(adapter.start(
        TunnelMode::Quick,
        32145,
        peer(&state, 45),
    )));

    let state = state.lock().unwrap();
    assert_eq!(state.stop_timeouts.len(), 3);
    assert_eq!(state.process_drops, 1);
}

#[test]
fn successful_start_exposes_transport_url_separately_from_peer_session() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let process = fake_process(&state, Ok(ready()));
    let mut running =
        expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer(&state, 46)));

    assert_eq!(
        running.transport_url().as_str(),
        "https://example-id.trycloudflare.com/"
    );
    running.stop(DEFAULT_STOP_TIMEOUT).unwrap();
}

#[test]
fn natural_process_exit_revokes_peer_session() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(&state, Ok(ready()));
    process.polls = VecDeque::from([Ok(Some(7))]);
    let mut running =
        expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer(&state, 47)));

    let terminal = running.wait_terminal(Duration::from_secs(1)).unwrap();

    assert_eq!(terminal, TerminalReason::ProcessExited(Some(7)));
    assert_eq!(state.lock().unwrap().session_revokes, 1);
}

#[test]
fn process_stop_failure_keeps_supervisor_state_for_retry() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(&state, Ok(ready()));
    process.stop_results = VecDeque::from([Err("stop timeout".into()), Ok(())]);
    let mut running =
        expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer(&state, 48)));

    assert!(matches!(
        running.stop(Duration::from_millis(20)),
        Err(TunnelError::Stop {
            process: Some(_),
            peer_session: None
        })
    ));
    running.stop(Duration::from_millis(20)).unwrap();

    let state = state.lock().unwrap();
    assert_eq!(state.stop_timeouts, [Duration::from_millis(20); 2]);
    assert_eq!(state.session_revokes, 1);
}

#[test]
fn peer_revoke_failure_keeps_supervisor_state_for_retry() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let process = fake_process(&state, Ok(ready()));
    let mut peer = peer(&state, 49);
    peer.revoke_results = VecDeque::from([Err("revoke failed".into()), Ok(())]);
    let mut running =
        expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer));

    assert!(matches!(
        running.stop(Duration::from_millis(20)),
        Err(TunnelError::Stop {
            process: None,
            peer_session: Some(_)
        })
    ));
    running.stop(Duration::from_millis(20)).unwrap();

    let state = state.lock().unwrap();
    assert_eq!(state.stop_timeouts, [Duration::from_millis(20)]);
    assert_eq!(state.session_revokes, 2);
}

#[test]
fn natural_exit_revoke_failure_can_be_retried_without_restarting_process() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(&state, Ok(ready()));
    process.polls = VecDeque::from([Ok(Some(9))]);
    let mut peer = peer(&state, 50);
    peer.revoke_results = VecDeque::from([Err("revoke failed".into()), Ok(())]);
    let mut running =
        expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer));

    thread::sleep(Duration::from_millis(30));
    running.stop(Duration::from_millis(20)).unwrap();

    let state = state.lock().unwrap();
    assert!(state.stop_timeouts.is_empty());
    assert_eq!(state.session_revokes, 2);
}

#[test]
fn drop_retries_cleanup_before_releasing_the_handle() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(&state, Ok(ready()));
    process.stop_results = VecDeque::from([
        Err("stop timeout 1".into()),
        Err("stop timeout 2".into()),
        Ok(()),
    ]);
    let running =
        expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer(&state, 51)));

    drop(running);

    let state = state.lock().unwrap();
    assert_eq!(state.stop_timeouts.len(), 3);
    assert_eq!(state.session_revokes, 1);
}

#[test]
fn drop_disconnects_and_rejoins_supervisor_after_bounded_retry_failure() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(&state, Ok(ready()));
    process.stop_results = VecDeque::from([
        Err("stop failed 1".into()),
        Err("stop failed 2".into()),
        Err("stop failed 3".into()),
        Err("disconnect cleanup failed".into()),
    ]);
    let mut peer = peer(&state, 52);
    peer.revoke_results = VecDeque::from([
        Err("revoke failed 1".into()),
        Err("revoke failed 2".into()),
        Err("revoke failed 3".into()),
        Err("disconnect cleanup failed".into()),
    ]);
    let running = expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer));

    drop(running);

    let state = state.lock().unwrap();
    assert_eq!(state.stop_timeouts.len(), 4);
    assert_eq!(state.session_revokes, 4);
    assert_eq!(state.process_drops, 1);
    assert_eq!(state.peer_session_drops, 1);
}

#[test]
fn poll_error_stops_process_and_revokes_peer_before_reporting_terminal_reason() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let mut process = fake_process(&state, Ok(ready()));
    process.polls = VecDeque::from([Err("wait failed".into())]);
    process.stop_results = VecDeque::from([Err("monitor cleanup timed out".into()), Ok(())]);
    let mut running =
        expect_running(adapter(&state, process).start(TunnelMode::Quick, 32145, peer(&state, 53)));

    let terminal = running.wait_terminal(Duration::from_secs(1)).unwrap();

    assert_eq!(
        terminal,
        TerminalReason::ProcessMonitorFailed {
            error: "wait failed".into(),
            process_cleanup_error: Some("monitor cleanup timed out".into()),
            peer_session_cleanup_error: None,
        }
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(state.stop_timeouts, [DEFAULT_STOP_TIMEOUT]);
        assert_eq!(state.session_revokes, 1);
    }

    running.stop(Duration::from_millis(20)).unwrap();

    let state = state.lock().unwrap();
    assert_eq!(
        state.stop_timeouts,
        [DEFAULT_STOP_TIMEOUT, Duration::from_millis(20)]
    );
    assert_eq!(state.session_revokes, 1);
}

#[cfg(windows)]
#[test]
fn windows_launcher_uses_create_no_window() {
    assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
}
