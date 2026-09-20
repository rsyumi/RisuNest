//! Owns only the cloudflared child started for this daemon's managed mode.
use crate::{store::Store, Error, Result};
use serde::Serialize;
use std::{net::SocketAddr, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::{mpsc, watch, Notify},
};

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    pub phase: &'static str,
    pub endpoint: Option<String>,
    pub error: Option<&'static str>,
    pub logs: Vec<String>,
}

#[derive(Default)]
struct Readiness {
    endpoint: Option<String>,
    registered: bool,
}
impl Readiness {
    fn observe(&mut self, line: &[u8]) -> Option<String> {
        let value: serde_json::Value = serde_json::from_slice(line).ok()?;
        let message = value.get("message")?.as_str()?;
        if message == "Registered tunnel connection" {
            self.registered = true;
        }
        if let Some(start) = message.find("https://") {
            let candidate: String = message[start..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '|')
                .collect();
            if let Ok(url) = risunest_sync_connect::validate_endpoint(&candidate, false) {
                let host = url.host_str().unwrap_or("");
                let name = host.strip_suffix(".trycloudflare.com").unwrap_or("");
                if !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                    && url.port().is_none()
                    && url.path() == "/"
                {
                    self.endpoint = Some(format!("https://{host}"));
                }
            }
        }
        self.registered.then(|| self.endpoint.clone()).flatten()
    }
}

fn cloudflared_args(origin: SocketAddr) -> Vec<String> {
    ["tunnel", "--no-autoupdate", "--output", "json", "--url"]
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(format!("http://{origin}")))
        .collect()
}

async fn read_lines(reader: impl AsyncRead + Unpin, tx: mpsc::Sender<Vec<u8>>) {
    let mut reader = BufReader::new(reader);
    loop {
        // read_until alone could allocate an unbounded malicious child line.
        let mut line = Vec::new();
        loop {
            let available = match reader.fill_buf().await {
                Ok(v) => v,
                Err(_) => return,
            };
            if available.is_empty() {
                if !line.is_empty() {
                    let _ = tx.send(line).await;
                }
                return;
            }
            let used = available
                .iter()
                .position(|v| *v == b'\n')
                .map_or(available.len(), |i| i + 1);
            if line.len() + used > 8192 {
                return;
            }
            let done = available[used - 1] == b'\n';
            line.extend_from_slice(&available[..used]);
            reader.consume(used);
            if done {
                break;
            }
        }
        if tx.send(line).await.is_err() {
            return;
        }
    }
}

pub async fn supervise(
    store: Arc<Store>,
    origin: SocketAddr,
    changed: Arc<Notify>,
    status: watch::Sender<TunnelStatus>,
    mut stop: watch::Receiver<bool>,
) {
    let executable = match store.managed_cloudflared() {
        Ok(Some(path)) => path,
        Ok(None) => {
            status.send_replace(TunnelStatus {
                phase: "external",
                endpoint: None,
                error: None,
                ..Default::default()
            });
            return;
        }
        Err(error) => {
            status.send_replace(TunnelStatus {
                phase: "failed",
                endpoint: None,
                error: Some(error.code),
                ..Default::default()
            });
            return;
        }
    };
    let mut failures = 0u32;
    loop {
        if *stop.borrow() {
            return;
        }
        status.send_modify(|value| {
            value.phase = "starting";
            value.endpoint = None;
        });
        let outcome = run_child(&executable, &store, origin, &changed, &status, stop.clone()).await;
        if *stop.borrow() {
            status.send_replace(TunnelStatus {
                phase: "stopped",
                endpoint: None,
                error: None,
                ..Default::default()
            });
            return;
        }
        let error = outcome.err().map_or("tunnel-exited", |e| e.code);
        status.send_modify(|value| {
            value.phase = "failed";
            value.endpoint = None;
            value.error = Some(error);
        });
        failures = failures.saturating_add(1);
        let delay = Duration::from_secs((1u64 << failures.min(6)).min(60));
        tokio::select! { _ = stop.changed() => return, _ = tokio::time::sleep(delay) => () }
    }
}

fn append_log(status: &watch::Sender<TunnelStatus>, line: &str) {
    let line: String = line
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .take(4096)
        .collect();
    status.send_modify(|value| {
        value.logs.push(line);
        while value.logs.len() > 64 || value.logs.iter().map(String::len).sum::<usize>() > 32 * 1024
        {
            value.logs.remove(0);
        }
    });
}

async fn run_child(
    executable: &std::path::Path,
    store: &Store,
    origin: SocketAddr,
    changed: &Notify,
    status: &watch::Sender<TunnelStatus>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let mut command = Command::new(executable);
    command
        .args(cloudflared_args(origin))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let mut child = command.spawn().map_err(|error| {
        append_log(status, &format!("Failed to start cloudflared: {error}"));
        Error::new("tunnel-start-failed", 503)
    })?;
    #[cfg(windows)]
    let _job = match crate::tunnel_job::TunnelJob::attach(&child) {
        Ok(job) => job,
        Err(error) => {
            append_log(
                status,
                &format!("Failed to manage cloudflared process: {error:?}"),
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(Error::new("tunnel-job-unavailable", 503));
        }
    };
    let (tx, mut rx) = mpsc::channel(16);
    let stdout = tokio::spawn(read_lines(
        child
            .stdout
            .take()
            .ok_or(Error::new("tunnel-output-unavailable", 503))?,
        tx.clone(),
    ));
    let stderr = tokio::spawn(read_lines(
        child
            .stderr
            .take()
            .ok_or(Error::new("tunnel-output-unavailable", 503))?,
        tx,
    ));
    let mut readiness = Readiness::default();
    let deadline = tokio::time::sleep(Duration::from_secs(90));
    tokio::pin!(deadline);
    let mut ready = false;
    let outcome = loop {
        tokio::select! {
            _ = stop.changed() => break Ok(()),
            _ = &mut deadline, if !ready => break Err(Error::new("tunnel-readiness-timeout", 503)),
            exit = child.wait() => {
                append_log(status, &format!("cloudflared exited: {exit:?}"));
                break Err(Error::new("tunnel-exited", 503));
            },
            line = rx.recv() => match line {
                Some(line) => {
                    append_log(status, &String::from_utf8_lossy(&line));
                    if let Some(endpoint) = readiness.observe(&line) {
                    if !ready {
                        if let Err(error) = store.observe_tunnel_endpoint(&endpoint) { break Err(error); }
                        ready = true;
                        status.send_modify(|value| {
                            value.phase = "connected";
                            value.endpoint = Some(endpoint);
                            value.error = None;
                        });
                        changed.notify_one();
                    }
                }},
                None => break Err(Error::new("tunnel-output-unavailable", 503)),
            }
        }
    };
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(line) = rx.recv().await {
            append_log(status, &String::from_utf8_lossy(&line));
        }
    })
    .await;
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_created_url_and_registered_edge_and_rejects_lookalikes() {
        let mut state = Readiness::default();
        assert!(state
            .observe(br#"{"message":"| https://synthetic-tunnel.trycloudflare.com |"}"#)
            .is_none());
        assert_eq!(
            state
                .observe(br#"{"message":"Registered tunnel connection"}"#)
                .unwrap(),
            "https://synthetic-tunnel.trycloudflare.com"
        );
        for message in [
            "https://sync.example",
            "https://good.trycloudflare.com.evil",
            "https://good.trycloudflare.com/path",
            "http://good.trycloudflare.com",
            "https://trycloudflare.com",
            "https://good.trycloudflare.com@evil.example",
            "https://good.trycloudflare.com?secret=x",
            "https://good.trycloudflare.com#fragment",
        ] {
            let mut state = Readiness {
                registered: true,
                endpoint: None,
            };
            assert!(state
                .observe(
                    serde_json::json!({"message":message})
                        .to_string()
                        .as_bytes()
                )
                .is_none());
        }
    }
    #[test]
    fn pinned_cloudflared_command_uses_json_output_flag() {
        assert_eq!(
            cloudflared_args("127.0.0.1:3210".parse().unwrap()),
            [
                "tunnel",
                "--no-autoupdate",
                "--output",
                "json",
                "--url",
                "http://127.0.0.1:3210",
            ]
        );
    }
    #[tokio::test]
    async fn child_output_is_bounded_without_a_newline() {
        let bytes = vec![b'x'; 8193];
        let (tx, mut rx) = mpsc::channel(1);
        read_lines(bytes.as_slice(), tx).await;
        assert!(rx.recv().await.is_none());
        let (tx, mut rx) = mpsc::channel(1);
        read_lines(&b"final error without newline"[..], tx).await;
        assert_eq!(rx.recv().await.unwrap(), b"final error without newline");
    }

    #[test]
    fn diagnostics_are_bounded_and_remove_terminal_controls() {
        let (status, _) = watch::channel(TunnelStatus::default());
        for _ in 0..100 {
            append_log(&status, &"x".repeat(8192));
        }
        append_log(&status, "\u{1b}[31mfailed\r\n");
        let value = status.borrow();
        assert!(value.logs.len() <= 64);
        assert!(value.logs.iter().map(String::len).sum::<usize>() <= 32 * 1024);
        assert!(value.logs.last().unwrap().contains("failed"));
        assert!(!value.logs.last().unwrap().contains('\u{1b}'));
    }
}
