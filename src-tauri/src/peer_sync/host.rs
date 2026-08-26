use super::{protocol::CloneManifest, PeerSyncError, PreparedCloneSession};
use crate::asset_repository::PayloadCas;
use std::{
    collections::BTreeMap,
    io::{self, Read, Seek, SeekFrom},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

struct HostShared {
    session: PreparedCloneSession,
    faults: Mutex<Vec<RangeFault>>,
    range_requests: Mutex<BTreeMap<(String, u64), usize>>,
}

enum RangeFault {
    Disconnect {
        object: String,
        offset: u64,
        after_bytes: u64,
    },
    Corrupt {
        object: String,
        offset: u64,
    },
}

pub struct LoopbackCloneHost {
    shared: Arc<HostShared>,
    address: SocketAddr,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), PeerSyncError>>>,
}

impl LoopbackCloneHost {
    pub fn start(session: PreparedCloneSession) -> Result<Self, PeerSyncError> {
        let server = Server::http((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
        let address = server.server_addr().to_ip().ok_or_else(|| {
            PeerSyncError::Transport("loopback server has no IP address".to_owned())
        })?;
        if !address.ip().is_loopback() {
            return Err(PeerSyncError::Protocol(
                "diagnostic clone server must bind loopback only".to_owned(),
            ));
        }
        let shared = Arc::new(HostShared {
            session,
            faults: Mutex::new(Vec::new()),
            range_requests: Mutex::new(BTreeMap::new()),
        });
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stopped = Arc::clone(&stopped);
        let thread = thread::spawn(move || serve(server, thread_shared, thread_stopped));
        Ok(Self {
            shared,
            address,
            stopped,
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn session_url(&self) -> String {
        format!(
            "http://{}/v1/sessions/{}",
            self.address,
            self.shared.session.manifest().session_id
        )
    }

    pub fn manifest(&self) -> &CloneManifest {
        self.shared.session.manifest()
    }

    #[cfg(test)]
    pub fn disconnect_once(&self, object: &str, offset: u64, after_bytes: u64) {
        self.shared
            .faults
            .lock()
            .unwrap()
            .push(RangeFault::Disconnect {
                object: object.to_owned(),
                offset,
                after_bytes,
            });
    }

    #[cfg(test)]
    pub fn corrupt_once(&self, object: &str, offset: u64) {
        self.shared
            .faults
            .lock()
            .unwrap()
            .push(RangeFault::Corrupt {
                object: object.to_owned(),
                offset,
            });
    }

    #[cfg(test)]
    pub fn range_request_count(&self, object: &str, offset: u64) -> usize {
        self.shared
            .range_requests
            .lock()
            .unwrap()
            .get(&(object.to_owned(), offset))
            .copied()
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn total_range_requests(&self, object: &str) -> usize {
        self.shared
            .range_requests
            .lock()
            .unwrap()
            .iter()
            .filter(|((hash, _), _)| hash == object)
            .map(|(_, count)| count)
            .sum()
    }

    pub fn shutdown(mut self) -> Result<(), PeerSyncError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), PeerSyncError> {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| {
                PeerSyncError::Transport("loopback server thread panicked".to_owned())
            })??;
        }
        Ok(())
    }
}

impl Drop for LoopbackCloneHost {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn serve(
    server: Server,
    shared: Arc<HostShared>,
    stopped: Arc<AtomicBool>,
) -> Result<(), PeerSyncError> {
    while !stopped.load(Ordering::SeqCst) {
        let request = server
            .recv_timeout(Duration::from_millis(50))
            .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
        let Some(request) = request else {
            continue;
        };
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let _ = handle_request(request, &shared);
    }
    Ok(())
}

fn handle_request(request: Request, shared: &HostShared) -> Result<(), PeerSyncError> {
    let session_prefix = format!("/v1/sessions/{}", shared.session.manifest().session_id);
    if request.url() == format!("{session_prefix}/manifest") {
        if request.method() != &Method::Get {
            return respond_empty(request, 405);
        }
        let headers = vec![
            header("content-type", "application/json")?,
            header("etag", &quoted(shared.session.manifest_id()))?,
        ];
        let response = Response::new(
            StatusCode(200),
            headers,
            io::Cursor::new(shared.session.manifest_bytes().to_vec()),
            Some(shared.session.manifest_bytes().len()),
            None,
        )
        .with_chunked_threshold(usize::MAX);
        return request
            .respond(response)
            .map_err(|error| PeerSyncError::Transport(error.to_string()));
    }

    let object_prefix = format!("{session_prefix}/objects/");
    let Some(object_hash) = request
        .url()
        .strip_prefix(&object_prefix)
        .map(str::to_owned)
    else {
        return respond_empty(request, 404);
    };
    if object_hash.contains('/') {
        return respond_empty(request, 404);
    }
    let Some(descriptor) = shared.session.manifest().objects.get(&object_hash) else {
        return respond_empty(request, 404);
    };

    match request.method() {
        Method::Head => {
            let headers = vec![
                header("content-length", &descriptor.size.to_string())?,
                header("accept-ranges", "bytes")?,
                header("etag", &quoted(&object_hash))?,
            ];
            request
                .respond(
                    Response::new(
                        StatusCode(200),
                        headers,
                        io::empty(),
                        Some(descriptor.size as usize),
                        None,
                    )
                    .with_chunked_threshold(usize::MAX),
                )
                .map_err(|error| PeerSyncError::Transport(error.to_string()))
        }
        Method::Get => serve_range(request, shared, &object_hash),
        _ => respond_empty(request, 405),
    }
}

fn serve_range(
    request: Request,
    shared: &HostShared,
    object_hash: &str,
) -> Result<(), PeerSyncError> {
    let range_headers: Vec<_> = request
        .headers()
        .iter()
        .filter(|candidate| candidate.field.equiv("range"))
        .collect();
    if range_headers.len() != 1 {
        return respond_empty(request, 416);
    }
    let Some((start, end)) = parse_single_range(range_headers[0].value.as_str()) else {
        return respond_empty(request, 416);
    };
    let descriptor = &shared.session.manifest().objects[object_hash];
    let Some(chunk) = descriptor
        .chunks
        .iter()
        .find(|chunk| chunk.offset == start && chunk.offset + chunk.size - 1 == end)
    else {
        return respond_empty(request, 416);
    };
    *shared
        .range_requests
        .lock()
        .unwrap()
        .entry((object_hash.to_owned(), start))
        .or_default() += 1;

    let physical_hash = shared
        .session
        .physical_hash(object_hash)
        .ok_or_else(|| PeerSyncError::Storage("session object mapping is missing".to_owned()))?;
    let cas = PayloadCas::new(shared.session.root())?;
    let mut file = cas
        .open_object(physical_hash)?
        .ok_or_else(|| PeerSyncError::Storage("session object file is missing".to_owned()))?;
    file.seek(SeekFrom::Start(start))?;
    let fault = take_fault(shared, object_hash, start);
    let reader = FaultingReader::new(file.take(chunk.size), fault);
    let headers = vec![
        header("accept-ranges", "bytes")?,
        header("etag", &quoted(object_hash))?,
        header(
            "content-range",
            &format!("bytes {start}-{end}/{}", descriptor.size),
        )?,
    ];
    let response = Response::new(
        StatusCode(206),
        headers,
        reader,
        Some(chunk.size as usize),
        None,
    )
    .with_chunked_threshold(usize::MAX);
    request
        .respond(response)
        .map_err(|error| PeerSyncError::Transport(error.to_string()))
}

fn take_fault(shared: &HostShared, object: &str, offset: u64) -> Option<ActiveFault> {
    let mut faults = shared.faults.lock().unwrap();
    let index = faults.iter().position(|fault| match fault {
        RangeFault::Disconnect {
            object: candidate,
            offset: candidate_offset,
            ..
        }
        | RangeFault::Corrupt {
            object: candidate,
            offset: candidate_offset,
        } => candidate == object && *candidate_offset == offset,
    })?;
    match faults.remove(index) {
        RangeFault::Disconnect { after_bytes, .. } => Some(ActiveFault::Disconnect { after_bytes }),
        RangeFault::Corrupt { .. } => Some(ActiveFault::Corrupt),
    }
}

enum ActiveFault {
    Disconnect { after_bytes: u64 },
    Corrupt,
}

struct FaultingReader<R> {
    inner: R,
    fault: Option<ActiveFault>,
    transferred: u64,
    corrupted: bool,
}

impl<R> FaultingReader<R> {
    fn new(inner: R, fault: Option<ActiveFault>) -> Self {
        Self {
            inner,
            fault,
            transferred: 0,
            corrupted: false,
        }
    }
}

impl<R: Read> Read for FaultingReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if let Some(ActiveFault::Disconnect { after_bytes }) = &self.fault {
            if self.transferred >= *after_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "injected loopback disconnect",
                ));
            }
            let remaining = (*after_bytes - self.transferred) as usize;
            let limit = output.len().min(remaining);
            let read = self.inner.read(&mut output[..limit])?;
            self.transferred += read as u64;
            return Ok(read);
        }
        let read = self.inner.read(output)?;
        if read != 0 && matches!(self.fault, Some(ActiveFault::Corrupt)) && !self.corrupted {
            output[0] ^= 0xff;
            self.corrupted = true;
        }
        self.transferred += read as u64;
        Ok(read)
    }
}

fn parse_single_range(value: &str) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || value.contains(char::is_whitespace) {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() || end.is_empty() {
        return None;
    }
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    (start <= end).then_some((start, end))
}

fn quoted(value: &str) -> String {
    format!("\"{value}\"")
}

fn header(name: &str, value: &str) -> Result<Header, PeerSyncError> {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .map_err(|_| PeerSyncError::Protocol("invalid HTTP response header".to_owned()))
}

fn respond_empty(request: Request, status: u16) -> Result<(), PeerSyncError> {
    request
        .respond(Response::empty(StatusCode(status)))
        .map_err(|error| PeerSyncError::Transport(error.to_string()))
}
