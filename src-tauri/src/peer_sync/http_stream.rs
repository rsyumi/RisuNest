use super::PeerSyncError;
use reqwest::{
    header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE},
    StatusCode, Url,
};
use std::time::{Duration, Instant};

const RANGE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
const RANGE_COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(super) struct HttpRangeStream {
    client: reqwest::Client,
    no_progress_timeout: Duration,
}

impl HttpRangeStream {
    pub(super) fn new(connect_timeout: Duration) -> Result<Self, PeerSyncError> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            client,
            no_progress_timeout: DEFAULT_NO_PROGRESS_TIMEOUT,
        })
    }

    #[cfg(test)]
    fn with_no_progress_timeout(mut self, timeout: Duration) -> Self {
        self.no_progress_timeout = timeout;
        self
    }

    pub(super) fn read(
        &self,
        url: Url,
        bearer: Option<&str>,
        object: &str,
        start: u64,
        end: u64,
        total: Option<u64>,
        is_cancelled: &dyn Fn() -> bool,
        on_bytes: &mut dyn FnMut(&[u8]) -> Result<(), PeerSyncError>,
    ) -> Result<u64, PeerSyncError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(transport_error)?;
        runtime.block_on(self.read_async(
            url,
            bearer,
            object,
            start,
            end,
            total,
            is_cancelled,
            on_bytes,
        ))
    }

    async fn read_async(
        &self,
        url: Url,
        bearer: Option<&str>,
        object: &str,
        start: u64,
        end: u64,
        total: Option<u64>,
        is_cancelled: &dyn Fn() -> bool,
        on_bytes: &mut dyn FnMut(&[u8]) -> Result<(), PeerSyncError>,
    ) -> Result<u64, PeerSyncError> {
        if is_cancelled() {
            return Err(PeerSyncError::Cancelled);
        }
        let expected_size = end
            .checked_sub(start)
            .and_then(|size| size.checked_add(1))
            .ok_or_else(|| PeerSyncError::Protocol("invalid clone range".to_owned()))?;
        let request = self
            .client
            .get(url)
            .header(RANGE, format!("bytes={start}-{end}"));
        let request = match bearer {
            Some(bearer) => request.bearer_auth(bearer),
            None => request,
        };
        let mut last_progress = Instant::now();
        let mut send = Box::pin(request.send());
        let mut response = loop {
            match tokio::time::timeout(RANGE_POLL_INTERVAL, &mut send).await {
                Ok(result) => break result.map_err(transport_error)?,
                Err(_) => self.check_wait(is_cancelled, last_progress)?,
            }
        };
        validate_response(&response, object, start, end, total)?;
        last_progress = Instant::now();

        let mut received = 0_u64;
        loop {
            let mut next = Box::pin(response.chunk());
            let chunk = loop {
                match tokio::time::timeout(RANGE_POLL_INTERVAL, &mut next).await {
                    Ok(result) => break result.map_err(transport_error)?,
                    Err(_) => self.check_wait(is_cancelled, last_progress)?,
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.is_empty() {
                continue;
            }
            last_progress = Instant::now();
            for bytes in chunk.chunks(RANGE_COPY_BUFFER_BYTES) {
                received = received.checked_add(bytes.len() as u64).ok_or_else(|| {
                    PeerSyncError::Protocol("clone range byte count overflow".to_owned())
                })?;
                if received > expected_size {
                    return Err(PeerSyncError::Protocol(
                        "range response exceeded the requested clone chunk".to_owned(),
                    ));
                }
                on_bytes(bytes)?;
            }
            if is_cancelled() {
                return Err(PeerSyncError::Cancelled);
            }
        }
        if received != expected_size {
            return Err(PeerSyncError::Transport(
                "range response ended before the requested clone chunk".to_owned(),
            ));
        }
        Ok(received)
    }

    fn check_wait(
        &self,
        is_cancelled: &dyn Fn() -> bool,
        last_progress: Instant,
    ) -> Result<(), PeerSyncError> {
        if is_cancelled() {
            return Err(PeerSyncError::Cancelled);
        }
        if last_progress.elapsed() >= self.no_progress_timeout {
            return Err(PeerSyncError::Transport(
                "clone range made no progress before its deadline".to_owned(),
            ));
        }
        Ok(())
    }
}

fn validate_response(
    response: &reqwest::Response,
    object: &str,
    start: u64,
    end: u64,
    total: Option<u64>,
) -> Result<(), PeerSyncError> {
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(PeerSyncError::Transport(format!(
            "range request returned {}",
            response.status()
        )));
    }
    let expected_size = end - start + 1;
    let content_length = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok());
    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok());
    let expected_etag = format!("\"{object}\"");
    let range_prefix = format!("bytes {start}-{end}/");
    let valid_range = content_range.is_some_and(|value| {
        value.strip_prefix(&range_prefix).is_some_and(|value| {
            value.parse::<u64>().ok().is_some_and(|received_total| {
                received_total > end
                    && total
                        .map(|expected_total| received_total == expected_total)
                        .unwrap_or(true)
            })
        })
    });
    if content_length != Some(expected_size) || etag != Some(expected_etag.as_str()) || !valid_range
    {
        return Err(PeerSyncError::Protocol(
            "range response does not match the requested immutable chunk".to_owned(),
        ));
    }
    Ok(())
}

fn transport_error(error: impl std::fmt::Display) -> PeerSyncError {
    PeerSyncError::Transport(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
    };

    const OBJECT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn range_server(
        size: u64,
        body: impl FnOnce(&mut TcpStream) + Send + 'static,
    ) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let end = size - 1;
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {size}\r\nContent-Range: bytes 0-{end}/{size}\r\nETag: \"{OBJECT}\"\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();
            body(&mut stream);
        });
        (
            Url::parse(&format!("http://{address}/objects/{OBJECT}")).unwrap(),
            thread,
        )
    }

    #[test]
    fn progressing_slow_eight_megabyte_range_has_no_total_deadline() {
        let size = 8 * 1024 * 1024_u64;
        let (url, server) = range_server(size, move |stream| {
            let bytes = [7_u8; RANGE_COPY_BUFFER_BYTES];
            for _ in 0..size / bytes.len() as u64 {
                stream.write_all(&bytes).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });
        let stream = HttpRangeStream::new(Duration::from_secs(1))
            .unwrap()
            .with_no_progress_timeout(Duration::from_millis(200));
        let mut received = 0_u64;
        let mut maximum = 0_usize;

        stream
            .read(
                url,
                None,
                OBJECT,
                0,
                size - 1,
                Some(size),
                &|| false,
                &mut |bytes| {
                    received += bytes.len() as u64;
                    maximum = maximum.max(bytes.len());
                    Ok(())
                },
            )
            .unwrap();
        server.join().unwrap();

        assert_eq!(received, size);
        assert!(maximum <= RANGE_COPY_BUFFER_BYTES);
    }

    #[test]
    fn stalled_range_fails_on_the_no_progress_deadline() {
        let (url, server) = range_server(1, |stream| {
            thread::sleep(Duration::from_millis(500));
            let _ = stream.write_all(&[1]);
        });
        let stream = HttpRangeStream::new(Duration::from_secs(1))
            .unwrap()
            .with_no_progress_timeout(Duration::from_millis(150));
        let started = Instant::now();

        let error = stream
            .read(url, None, OBJECT, 0, 0, Some(1), &|| false, &mut |_| Ok(()))
            .unwrap_err();
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert!(
            matches!(error, PeerSyncError::Transport(message) if message.contains("no progress"))
        );
        assert!(elapsed < Duration::from_millis(400));
    }

    #[test]
    fn cancellation_interrupts_a_stalled_range_poll() {
        let (url, server) = range_server(1, |stream| {
            thread::sleep(Duration::from_millis(500));
            let _ = stream.write_all(&[1]);
        });
        let stream = HttpRangeStream::new(Duration::from_secs(1))
            .unwrap()
            .with_no_progress_timeout(Duration::from_secs(5));
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();

        let error = stream
            .read(
                url,
                None,
                OBJECT,
                0,
                0,
                Some(1),
                &|| cancelled.load(Ordering::SeqCst),
                &mut |_| Ok(()),
            )
            .unwrap_err();
        let elapsed = started.elapsed();
        canceller.join().unwrap();
        server.join().unwrap();

        assert!(matches!(error, PeerSyncError::Cancelled));
        assert!(elapsed < Duration::from_millis(400));
    }
}
