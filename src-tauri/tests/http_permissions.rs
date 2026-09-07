use std::collections::BTreeMap;

use serde_json::{json, Value};
use tauri::{
    ipc::{CallbackFn, InvokeBody, InvokeResponseBody, RuntimeAuthority},
    test::{get_ipc_response, mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY},
    utils::{
        acl::{capability::Capability, resolved::Resolved},
        platform::Target,
    },
    webview::InvokeRequest,
    WebviewWindow, WebviewWindowBuilder,
};

fn http_window(platform: Target) -> WebviewWindow<MockRuntime> {
    let manifests = serde_json::from_str(include_str!(concat!(
        env!("OUT_DIR"),
        "/acl-manifests.json"
    )))
    .unwrap();
    let capabilities = [
        include_str!("../capabilities/migrated.json"),
        include_str!("../capabilities/mobile.json"),
        include_str!("../capabilities/desktop.json"),
    ]
    .into_iter()
    .map(|source| {
        let mut value: Value = serde_json::from_str(source).unwrap();
        // Exercise each platform's real HTTP permissions without unrelated plugins.
        value["permissions"]
            .as_array_mut()
            .unwrap()
            .retain(|permission| {
                permission
                    .as_str()
                    .or_else(|| permission["identifier"].as_str())
                    .is_some_and(|identifier| identifier.starts_with("http:"))
            });
        let capability: Capability = serde_json::from_value(value).unwrap();
        (capability.identifier.clone(), capability)
    })
    .collect::<BTreeMap<_, _>>();
    let resolved = Resolved::resolve(&manifests, capabilities, platform).unwrap();
    let mut context = mock_context(noop_assets());
    *context.runtime_authority_mut() = RuntimeAuthority::new(manifests, resolved);
    let app = mock_builder()
        .plugin(tauri_plugin_http::init())
        .build(context)
        .unwrap();
    WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap()
}

fn invoke(
    window: &WebviewWindow<MockRuntime>,
    command: &str,
    body: Value,
) -> Result<InvokeResponseBody, Value> {
    get_ipc_response(
        window,
        InvokeRequest {
            cmd: format!("plugin:http|{command}"),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: window.url().unwrap(),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.into(),
        },
    )
}

#[test]
fn all_http_urls_pass_the_native_scope_on_every_configured_platform() {
    for platform in [
        Target::Windows,
        Target::Linux,
        Target::MacOS,
        Target::Android,
    ] {
        let window = http_window(platform);
        for url in [
            "https://api.example.invalid",
            "https://api.example.invalid:443/v1/chat?model=test#result",
            "https://api.example.invalid:8443/v1/chat?model=test#result",
            "http://api.example.invalid:8080/v1/chat",
            "http://localhost:11434/api/chat",
            "http://127.0.0.1:5000/",
            "http://192.168.1.2:8000/v1/chat",
            "http://[::1]:11434/api/chat",
            "https://[2001:db8::1]:9443/a/b?x=1",
            "https://api.example.invalid:65535/a/b/c",
        ] {
            // `fetch` creates a request resource. Only `fetch_send` accesses the network.
            let response = invoke(
                &window,
                "fetch",
                json!({"clientConfig": {
                    "url": url, "method": "GET", "headers": []
                }}),
            );
            assert!(
                response.is_ok(),
                "{platform:?} rejected {url}: {response:?}"
            );
            let rid = response.unwrap().deserialize::<u32>().unwrap();
            invoke(&window, "fetch_cancel", json!({"rid": rid})).unwrap();
        }
    }
}

#[test]
fn caller_headers_reach_the_native_http_server() {
    use std::time::Duration;

    let listener = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/headers", listener.server_addr().to_ip().unwrap());
    let server = std::thread::spawn(move || {
        let request = listener
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .expect("local HTTP fixture did not receive a request");
        let headers = request
            .headers()
            .iter()
            .map(|header| format!("{header}\r\n").to_lowercase())
            .collect::<String>();
        request.respond(tiny_http::Response::empty(204)).unwrap();
        headers
    });
    let window = http_window(Target::Windows);
    let rid = invoke(
        &window,
        "fetch",
        json!({"clientConfig": {
            "url": url, "method": "GET", "headers": [
                ["Origin", "https://plugin.example.invalid"],
                ["Referer", "https://plugin.example.invalid/page"],
                ["Cookie", "fixture=synthetic"],
                ["Authorization", "Bearer synthetic"]
            ]
        }}),
    )
    .unwrap()
    .deserialize::<u32>()
    .unwrap();
    let response = invoke(&window, "fetch_send", json!({"rid": rid}));
    let request = server.join().unwrap();
    assert!(
        response.is_ok(),
        "native fetch failed: {response:?}; captured request: {request:?}"
    );
    for header in [
        "origin: https://plugin.example.invalid\r\n",
        "referer: https://plugin.example.invalid/page\r\n",
        "cookie: fixture=synthetic\r\n",
        "authorization: bearer synthetic\r\n",
    ] {
        assert!(
            request.contains(header),
            "missing {header:?} in {request:?}"
        );
    }
}
