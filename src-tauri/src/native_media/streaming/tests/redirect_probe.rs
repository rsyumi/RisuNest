//! Determine whether WebView2 re-enters an asset resolver on media Range reads.
use super::*;
use axum::{extract::State, routing::get};
use std::{
    borrow::Cow,
    io::Write,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::{run_return::EventLoopExtRunReturn, windows::EventLoopBuilderExtWindows},
    window::WindowBuilder,
};
use wry::{WebContext, WebViewBuilder};

#[derive(Clone)]
struct Probe {
    path: PathBuf,
    refresh: String,
    requests: Arc<AtomicUsize>,
    visited: Arc<std::sync::Mutex<Vec<String>>>,
}

#[test]
#[ignore = "manual isolated WebView redirect and media seek experiment"]
fn media_seek_refreshes_expired_remote_url() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("synthetic.wav");
    let data_len = 16 * 1024 * 1024u32;
    let mut file = fs::File::create(&path).unwrap();
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(data_len + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&8000u32.to_le_bytes());
    wav.extend_from_slice(&16000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    file.write_all(&wav).unwrap();
    file.set_len(u64::from(data_len) + 44).unwrap();
    drop(file);
    let remote = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    remote.set_nonblocking(true).unwrap();
    let remote_addr = remote.local_addr().unwrap();
    let gateway = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    gateway.set_nonblocking(true).unwrap();
    let gateway_addr = gateway.local_addr().unwrap();
    let probe = Probe {
        path,
        refresh: format!("http://{gateway_addr}/asset"),
        requests: Arc::new(AtomicUsize::new(0)),
        visited: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let observed = probe.clone();
    let remote_task = tauri::async_runtime::spawn(async move {
        axum::serve(
            tokio::net::TcpListener::from_std(remote).unwrap(),
            Router::new().fallback(remote_file).with_state(observed),
        )
        .await
        .unwrap();
    });
    let counter = probe.requests.clone();
    let gateway_task = tauri::async_runtime::spawn(async move {
        axum::serve(
            tokio::net::TcpListener::from_std(gateway).unwrap(),
            Router::new().route(
                "/asset",
                get(move || {
                    let counter = counter.clone();
                    async move {
                        let id = counter.fetch_add(1, Ordering::SeqCst) + 1;
                        Response::builder()
                            .status(307)
                            .header("location", format!("http://{remote_addr}/{id}"))
                            .header("access-control-allow-origin", "*")
                            .header("cache-control", "no-store")
                            .body(Body::empty())
                            .unwrap()
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let page = format!(
        r#"<html><body><audio id="a" crossorigin="anonymous" preload="auto" src="http://{gateway_addr}/asset"></audio><script>
        let a=document.querySelector('audio'),done=false;function end(s){{if(done)return;done=true;window.ipc.postMessage(s)}};
        a.onloadedmetadata=()=>setTimeout(()=>{{a.currentTime=900}},100);a.onseeked=()=>end(a.currentTime>890?'seek-ok':'seek-wrong');a.onerror=()=>end('media-error');setTimeout(()=>end('timeout'),7000);
        </script></body></html>"#
    );
    let mut events = EventLoopBuilder::<String>::with_user_event()
        .with_any_thread(true)
        .build();
    let proxy = events.create_proxy();
    let window = WindowBuilder::new()
        .with_visible(false)
        .build(&events)
        .unwrap();
    let profile = TempDir::new().unwrap();
    let mut context = WebContext::new(Some(profile.path().to_path_buf()));
    let _webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_custom_protocol("tauri".into(), move |_, _| {
            wry::http::Response::builder()
                .header("content-type", "text/html")
                .body(Cow::Owned(page.clone().into_bytes()))
                .unwrap()
        })
        .with_ipc_handler(move |request| {
            let _ = proxy.send_event(request.body().clone());
        })
        .with_url("tauri://localhost/")
        .build(&window)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(9);
    let mut result = String::new();
    events.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(deadline);
        if let Event::UserEvent(value) = event {
            result = value;
            *flow = ControlFlow::Exit;
        }
        if Instant::now() >= deadline {
            *flow = ControlFlow::Exit;
        }
    });
    println!(
        "REDIRECT_PROBE result={result} gateway_requests={} remote_requests={:?}",
        probe.requests.load(Ordering::SeqCst),
        probe.visited.lock().unwrap()
    );
    remote_task.abort();
    gateway_task.abort();
    assert_eq!(result, "seek-ok");
}

async fn remote_file(State(probe): State<Probe>, req: Request<Body>) -> Response<Body> {
    let token = req.uri().path().to_owned();
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let duplicate = {
        let mut visited = probe.visited.lock().unwrap();
        let duplicate = visited.iter().any(|v| v.starts_with(&format!("{token}:")));
        visited.push(format!("{token}:{range}"));
        duplicate
    };
    if duplicate {
        return Response::builder()
            .status(307)
            .header("location", &probe.refresh)
            .header("cache-control", "no-store")
            .header("access-control-allow-origin", "*")
            .body(Body::empty())
            .unwrap();
    }
    let total = fs::metadata(&probe.path).unwrap().len();
    let start = range
        .strip_prefix("bytes=")
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let mut file = tokio::fs::File::open(&probe.path).await.unwrap();
    use tokio::io::AsyncSeekExt;
    file.seek(std::io::SeekFrom::Start(start)).await.unwrap();
    let stream = futures::stream::try_unfold(file, |mut file| async move {
        let mut bytes = vec![0; 64 * 1024];
        let count = file.read(&mut bytes).await?;
        if count == 0 {
            return Ok::<_, io::Error>(None);
        }
        bytes.truncate(count);
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(Some((bytes, file)))
    });
    Response::builder()
        .status(if range.is_empty() { 200 } else { 206 })
        .header("content-type", "audio/wav")
        .header("content-length", total - start)
        .header(
            "content-range",
            format!("bytes {start}-{}/{total}", total - 1),
        )
        .header("accept-ranges", "bytes")
        .header("cache-control", "no-store")
        .header("access-control-allow-origin", "*")
        .body(Body::from_stream(stream))
        .unwrap()
}
