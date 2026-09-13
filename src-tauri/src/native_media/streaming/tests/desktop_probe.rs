//! Manual isolated WebView2 measurement, compiled only into the test binary.
use super::*;
use std::{
    borrow::Cow,
    io::{Seek, SeekFrom, Write},
    time::{Duration, Instant},
};
use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::{run_return::EventLoopExtRunReturn, windows::EventLoopBuilderExtWindows},
    window::WindowBuilder,
};
use wry::{WebContext, WebViewBuilder, WebViewBuilderExtWindows};

#[test]
#[ignore = "manual synthetic Windows WebView memory experiment"]
fn compare_file_display() {
    let buffered = std::env::var("RISUNEST_MEDIA_PROBE_MODE").as_deref() == Ok("buffered");
    let root = TempDir::new().unwrap();
    let dimension = 4096u32;
    let size = 54 + u64::from(dimension) * u64::from(dimension) * 4;
    super::super::super::tests::write_blob(root.path(), "assets/probe.bmp", &[], "image/bmp");
    let payload = root.path().join("assets/probe.bmp");
    let mut file = fs::OpenOptions::new().write(true).open(&payload).unwrap();
    file.set_len(size).unwrap();
    let mut header = vec![0u8; 54];
    header[..2].copy_from_slice(b"BM");
    header[2..6].copy_from_slice(&(size as u32).to_le_bytes());
    header[10..14].copy_from_slice(&54u32.to_le_bytes());
    header[14..18].copy_from_slice(&40u32.to_le_bytes());
    header[18..22].copy_from_slice(&dimension.to_le_bytes());
    header[22..26].copy_from_slice(&dimension.to_le_bytes());
    header[26..28].copy_from_slice(&1u16.to_le_bytes());
    header[28..30].copy_from_slice(&32u16.to_le_bytes());
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&header).unwrap();
    drop(file);
    fs::write(root.path().join("blobstore/metadata").join(format!("{}.json", hex::encode("assets/probe.bmp"))),
        serde_json::to_vec(&serde_json::json!({"key":"assets/probe.bmp","kind":"asset","mime":"image/bmp","size":size})).unwrap()).unwrap();
    let server = MediaServer::start(root.path().to_path_buf()).unwrap();
    let url = if buffered {
        "http://risuasset.localhost/probe".to_owned()
    } else {
        format!("{}{}", server.base_url, hex::encode("assets/probe.bmp"))
    };
    let page = format!(
        r#"<html><body><script>
        let i=new Image();i.crossOrigin='anonymous';i.onload=()=>{{let c=document.createElement('canvas');c.width=c.height=1;c.getContext('2d').drawImage(i,0,0);let p=c.getContext('2d').getImageData(0,0,1,1).data;
        window.ipc.postMessage(JSON.stringify({{state:p[0]===0&&i.naturalWidth===4096?'ok':'bad-pixel',elapsed:performance.now()-start}}));}};
        i.onerror=()=>window.ipc.postMessage('image-failed');let start;setTimeout(()=>{{start=performance.now();i.src='{url}';document.body.append(i)}},2000);
        </script></body></html>"#
    );
    let mut event_loop = EventLoopBuilder::<String>::with_user_event()
        .with_any_thread(true)
        .build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_visible(false)
        .build(&event_loop)
        .unwrap();
    let profile = TempDir::new().unwrap();
    let mut context = WebContext::new(Some(profile.path().to_path_buf()));
    let _webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_custom_protocol("tauri".into(), move |_, _| {
            wry::http::Response::builder()
                .header("Content-Type", "text/html")
                .body(Cow::Owned(page.clone().into_bytes()))
                .unwrap()
        })
        .with_custom_protocol("risuasset".into(), move |_, _| {
            wry::http::Response::builder()
                .header("Content-Type", "image/bmp")
                .header("Access-Control-Allow-Origin", "*")
                .header("Cache-Control", "no-store")
                .body(Cow::Owned(fs::read(&payload).unwrap()))
                .unwrap()
        })
        .with_https_scheme(false)
        .with_ipc_handler(move |request| {
            let _ = proxy.send_event(request.body().clone());
        })
        .with_url("tauri://localhost/")
        .build(&window)
        .unwrap();
    println!(
        "MEDIA_PROBE_READY pid={} buffered={buffered}",
        std::process::id()
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut result = None;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(deadline);
        if let Event::UserEvent(value) = event {
            println!("MEDIA_PROBE_RESULT:{value}");
            result = Some(value);
        }
        if Instant::now() >= deadline {
            *flow = ControlFlow::Exit;
        }
    });
    assert!(result.unwrap_or_default().contains("\"state\":\"ok\""));
}
