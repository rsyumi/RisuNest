use serde_json::{json, Value};
use std::{
    error::Error,
    time::{Duration, Instant},
};
use tauri_plugin_http::reqwest;

// Verification-only, fixed public endpoint. Never send a credential or retain a body.
pub async fn probe() -> Value {
    let mut results = Vec::new();
    for http1 in [false, true] {
        let started = Instant::now();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15));
        if http1 {
            builder = builder.http1_only();
        }
        let result = match builder.build() {
            Ok(client) => client.get("https://ollama.com/api/tags").send().await,
            Err(error) => Err(error),
        };
        let mut record = match result {
            Ok(response) => json!({
                "status": response.status().as_u16(),
                "http2": response.version() == reqwest::Version::HTTP_2,
            }),
            Err(error) => classify(&error),
        };
        record["http1Only"] = json!(http1);
        record["elapsedMs"] = json!(started.elapsed().as_millis());
        results.push(record);
    }
    json!(results)
}

fn classify(error: &reqwest::Error) -> Value {
    let mut chain = Vec::new();
    let mut source: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(current) = source {
        // Error descriptions may contain request URLs. Only fixed categories and
        // numeric OS errors leave this function, never descriptions/debug text.
        let message = current.to_string().to_lowercase();
        let categories: Vec<_> = [
            "dns",
            "certificate",
            "tls",
            "connect",
            "timeout",
            "timed out",
            "unreachable",
            "refused",
            "reset",
            "proxy",
        ]
        .into_iter()
        .filter(|category| message.contains(category))
        .collect();
        let os_code = current
            .downcast_ref::<std::io::Error>()
            .and_then(|e| e.raw_os_error());
        chain.push(json!({"categories": categories, "osCode": os_code}));
        source = current.source();
        if chain.len() == 8 {
            break;
        }
    }
    json!({"failed": true, "connect": error.is_connect(), "timeout": error.is_timeout(), "builder": error.is_builder(), "chain": chain})
}
