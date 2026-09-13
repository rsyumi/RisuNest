//! Shared synthetic wire fixture for provider tests. Not compiled into the product.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

pub(crate) struct WireRequest {
    pub headers: String,
    pub body: Vec<u8>,
}
pub(crate) enum Reply {
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Lost,
    DelayedHeaders,
    DelayedBody,
}
pub(crate) struct WireServer {
    pub url: url::Url,
    pub requests: Arc<Mutex<Vec<WireRequest>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl WireServer {
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = url::Url::parse(&format!(
            "http://{}/synthetic",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let records = requests.clone();
        let stop = stopped.clone();
        let worker = thread::spawn(move || {
            for reply in replies {
                let mut stream = loop {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("synthetic accept: {}", error.kind()),
                    }
                };
                // Windows accepts inherit FIONBIO from the polling listener.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request = read_request(&mut stream);
                records.lock().unwrap().push(request);
                match reply {
                    Reply::Lost => {}
                    Reply::DelayedHeaders => thread::sleep(Duration::from_millis(250)),
                    Reply::DelayedBody => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
                        );
                        thread::sleep(Duration::from_millis(250));
                    }
                    Reply::Http {
                        status,
                        headers,
                        body,
                    } => {
                        let mut head=format!("HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n",body.len());
                        for (name, value) in headers {
                            head.push_str(&format!("{name}: {value}\r\n"));
                        }
                        head.push_str("\r\n");
                        if stream.write_all(head.as_bytes()).is_ok() {
                            let _ = stream.write_all(&body);
                        }
                    }
                }
            }
        });
        Self {
            url,
            requests,
            stopped,
            worker: Some(worker),
        }
    }
}
fn read_request(stream: &mut TcpStream) -> WireRequest {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < 64 * 1024);
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    let headers = String::from_utf8(headers).unwrap();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    assert!(length <= 4 * 1024 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    WireRequest { headers, body }
}
impl Drop for WireServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}
