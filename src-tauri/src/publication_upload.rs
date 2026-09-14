use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use std::path::Path;
use tokio_util::io::ReaderStream;

const DATABASE_KEY: &str = "database/database.bin";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum OfficialPublicationCredential {
    RisuAuth { token: String },
    Bearer { token: String },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfficialPublicationUploadRequest {
    pub(crate) base_url: String,
    pub(crate) path: String,
    pub(crate) session: Option<String>,
    pub(crate) save_date: String,
    pub(crate) credential: OfficialPublicationCredential,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum OfficialPublicationUploadResult {
    Written {
        replacement_key: String,
        session: String,
        save_date: String,
        status: u16,
        bytes_uploaded: u64,
        warning: Option<String>,
        reload_session: bool,
    },
    NotModified {
        replacement_key: String,
        session: String,
        save_date: String,
        status: u16,
        bytes_uploaded: u64,
    },
    AuthWarning {
        session: String,
        save_date: String,
        status: u16,
        bytes_uploaded: u64,
        warning: Option<String>,
    },
    ReauthenticationNeeded {
        session: String,
        save_date: String,
        status: u16,
        bytes_uploaded: u64,
        warning: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "kebab-case")]
pub(crate) enum OfficialPublicationUploadError {
    InvalidRequest { message: String },
    Source { message: String },
    Transport { message: String },
    HttpStatus { status: u16, message: String },
    ResponseTooLarge,
    InvalidResponse { message: String },
}

impl std::fmt::Display for OfficialPublicationUploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest { message }
            | Self::Source { message }
            | Self::Transport { message }
            | Self::InvalidResponse { message } => formatter.write_str(message),
            Self::HttpStatus { status, message } => {
                write!(
                    formatter,
                    "official publication upload returned {status}: {message}"
                )
            }
            Self::ResponseTooLarge => {
                formatter.write_str("official publication response exceeds 64 KiB")
            }
        }
    }
}

impl std::error::Error for OfficialPublicationUploadError {}

fn endpoint(base_url: &str, path: &str) -> Result<String, OfficialPublicationUploadError> {
    let parsed = url::Url::parse(base_url).map_err(|error| {
        OfficialPublicationUploadError::InvalidRequest {
            message: format!("invalid official account base URL: {error}"),
        }
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(OfficialPublicationUploadError::InvalidRequest {
            message: "official account base URL must use HTTP or HTTPS".to_owned(),
        });
    }
    Ok(format!("{}{path}", base_url.trim_end_matches('/')))
}

fn authenticated_headers(
    credential: &OfficialPublicationCredential,
) -> Result<HeaderMap, OfficialPublicationUploadError> {
    let mut headers = HeaderMap::new();
    let (name, value) = match credential {
        OfficialPublicationCredential::RisuAuth { token } => (
            HeaderName::from_static("x-risu-auth"),
            token.as_str().to_owned(),
        ),
        OfficialPublicationCredential::Bearer { token } => {
            (AUTHORIZATION, format!("Bearer {token}"))
        }
    };
    let value = HeaderValue::from_str(&value).map_err(|_| {
        OfficialPublicationUploadError::InvalidRequest {
            message: "official account credential contains invalid header bytes".to_owned(),
        }
    })?;
    headers.insert(name, value);
    Ok(headers)
}

fn build_client() -> Result<reqwest::Client, OfficialPublicationUploadError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| OfficialPublicationUploadError::Transport {
            message: error.to_string(),
        })
}

async fn response_bytes(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, OfficialPublicationUploadError> {
    let mut bytes = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|error| OfficialPublicationUploadError::Transport {
                message: error.to_string(),
            })?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(OfficialPublicationUploadError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn response_text(bytes: Vec<u8>) -> Result<String, OfficialPublicationUploadError> {
    String::from_utf8(bytes).map_err(|error| OfficialPublicationUploadError::InvalidResponse {
        message: format!("official publication response is not UTF-8: {error}"),
    })
}

async fn acquire_session(
    client: &reqwest::Client,
    request: &OfficialPublicationUploadRequest,
) -> Result<String, OfficialPublicationUploadError> {
    let response = client
        .get(endpoint(
            &request.base_url,
            "/api/account/getsessionnumber",
        )?)
        .headers(authenticated_headers(&request.credential)?)
        .send()
        .await
        .map_err(|error| OfficialPublicationUploadError::Transport {
            message: error.to_string(),
        })?;
    let status = response.status().as_u16();
    let bytes = response_bytes(response).await?;
    if !(200..300).contains(&status) {
        return Err(OfficialPublicationUploadError::HttpStatus {
            status,
            message: response_text(bytes)?,
        });
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        OfficialPublicationUploadError::InvalidResponse {
            message: format!("invalid official publication session response: {error}"),
        }
    })?;
    match &value["sessionNumber"] {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(OfficialPublicationUploadError::InvalidResponse {
            message: "official publication session response has no sessionNumber".to_owned(),
        }),
    }
}

#[cfg(test)]
pub(crate) async fn upload_file_attempt(
    request: OfficialPublicationUploadRequest,
) -> Result<OfficialPublicationUploadResult, OfficialPublicationUploadError> {
    let file = std::fs::File::open(Path::new(&request.path)).map_err(|error| {
        OfficialPublicationUploadError::Source {
            message: error.to_string(),
        }
    })?;
    let bytes = file
        .metadata()
        .map_err(|error| OfficialPublicationUploadError::Source {
            message: error.to_string(),
        })?
        .len();
    upload_open_file_attempt(request, file, bytes).await
}

pub(crate) async fn upload_open_file_attempt(
    request: OfficialPublicationUploadRequest,
    file: std::fs::File,
    bytes_uploaded: u64,
) -> Result<OfficialPublicationUploadResult, OfficialPublicationUploadError> {
    let client = build_client()?;
    let session = match &request.session {
        Some(session) if !session.is_empty() => session.clone(),
        _ => acquire_session(&client, &request).await?,
    };
    let file = tokio::fs::File::from_std(file);
    let mut headers = authenticated_headers(&request.credential)?;
    for (name, value) in [
        (CONTENT_TYPE, "application/octet-stream"),
        (HeaderName::from_static("x-risu-key"), DATABASE_KEY),
        (HeaderName::from_static("x-format"), "nocheck"),
        (HeaderName::from_static("x-risu-session"), session.as_str()),
        (
            HeaderName::from_static("x-risu-save-date"),
            request.save_date.as_str(),
        ),
    ] {
        headers.insert(
            name,
            HeaderValue::from_str(value).map_err(|_| {
                OfficialPublicationUploadError::InvalidRequest {
                    message: "official publication header contains invalid bytes".to_owned(),
                }
            })?,
        );
    }
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&bytes_uploaded.to_string()).expect("file length is a valid header"),
    );
    let response = client
        .post(endpoint(&request.base_url, "/api/account/write")?)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
        .send()
        .await
        .map_err(|error| OfficialPublicationUploadError::Transport {
            message: error.to_string(),
        })?;
    let status = response.status().as_u16();
    let warning_status =
        response.headers().get("x-risu-status") == Some(&HeaderValue::from_static("warn"));
    let json_response = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or_default().trim();
            media_type.eq_ignore_ascii_case("application/json")
        });
    if status == 304 {
        return Ok(OfficialPublicationUploadResult::NotModified {
            replacement_key: DATABASE_KEY.to_owned(),
            session,
            save_date: request.save_date,
            status,
            bytes_uploaded,
        });
    }
    if status == 403 {
        let warning = if json_response {
            let bytes = response_bytes(response).await?;
            serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("warning")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        } else {
            None
        };
        return Ok(if warning_status {
            OfficialPublicationUploadResult::AuthWarning {
                session,
                save_date: request.save_date,
                status,
                bytes_uploaded,
                warning,
            }
        } else {
            OfficialPublicationUploadResult::ReauthenticationNeeded {
                session,
                save_date: request.save_date,
                status,
                bytes_uploaded,
                warning,
            }
        });
    }
    let bytes = response_bytes(response).await?;
    let text = response_text(bytes)?;
    if !(200..300).contains(&status) {
        return Err(OfficialPublicationUploadError::HttpStatus {
            status,
            message: text,
        });
    }
    let (warning, reload_session) = if json_response {
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            OfficialPublicationUploadError::InvalidResponse {
                message: format!("invalid official publication JSON response: {error}"),
            }
        })?;
        (
            value
                .get("warning")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            value
                .get("reloadSession")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    } else {
        (None, false)
    };
    Ok(OfficialPublicationUploadResult::Written {
        replacement_key: text,
        session,
        save_date: request.save_date,
        status,
        bytes_uploaded,
        warning,
        reload_session,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        upload_file_attempt, upload_open_file_attempt, OfficialPublicationCredential,
        OfficialPublicationUploadError, OfficialPublicationUploadRequest,
        OfficialPublicationUploadResult, DATABASE_KEY, MAX_RESPONSE_BYTES,
    };
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use tempfile::TempDir;

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    struct MockResponse {
        status: u16,
        headers: &'static [(&'static str, &'static str)],
        body: &'static [u8],
    }

    fn read_request(stream: &mut TcpStream) -> RecordedRequest {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "request ended before headers completed");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let header_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let mut lines = header_text.split("\r\n");
        let mut request_line = lines.next().unwrap().split_whitespace();
        let method = request_line.next().unwrap().to_owned();
        let path = request_line.next().unwrap().to_owned();
        let mut headers = BTreeMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let content_length = headers
            .get("content-length")
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(0);
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "request ended before body completed");
            bytes.extend_from_slice(&chunk[..read]);
        }
        RecordedRequest {
            method,
            path,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    fn mock_server(
        responses: Vec<MockResponse>,
    ) -> (
        String,
        Arc<Mutex<Vec<RecordedRequest>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                recorded.lock().unwrap().push(read_request(&mut stream));
                write!(
                    stream,
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status,
                    response.body.len(),
                )
                .unwrap();
                for (name, value) in response.headers {
                    write!(stream, "{name}: {value}\r\n").unwrap();
                }
                write!(stream, "\r\n").unwrap();
                stream.write_all(response.body).unwrap();
                stream.flush().unwrap();
                stream.shutdown(Shutdown::Write).unwrap();
                let mut drain = [0u8; 256];
                while stream.read(&mut drain).unwrap_or(0) > 0 {}
            }
        });
        (format!("http://{address}"), requests, handle)
    }

    fn request(base_url: String, path: &Path) -> OfficialPublicationUploadRequest {
        OfficialPublicationUploadRequest {
            base_url,
            path: path.to_string_lossy().into_owned(),
            session: None,
            save_date: "1725000000123".to_owned(),
            credential: OfficialPublicationCredential::RisuAuth {
                token: "account-secret".to_owned(),
            },
        }
    }

    #[test]
    fn uploads_exact_file_bytes_after_acquiring_the_session() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("snapshot.risudat");
        let body = b"RISUSAVE\0exact body\x00\xff";
        std::fs::write(&source, body).unwrap();
        let (base_url, requests, server) = mock_server(vec![
            MockResponse {
                status: 200,
                headers: &[("content-type", "application/json")],
                body: br#"{"sessionNumber":42}"#,
            },
            MockResponse {
                status: 200,
                headers: &[("content-type", "text/plain")],
                body: b"database/database.bin",
            },
        ]);

        let result =
            tauri::async_runtime::block_on(upload_file_attempt(request(base_url, &source)))
                .unwrap();
        server.join().unwrap();

        assert_eq!(
            result,
            OfficialPublicationUploadResult::Written {
                replacement_key: "database/database.bin".to_owned(),
                session: "42".to_owned(),
                save_date: "1725000000123".to_owned(),
                status: 200,
                bytes_uploaded: body.len() as u64,
                warning: None,
                reload_session: false,
            }
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/account/getsessionnumber");
        assert_eq!(requests[0].headers["x-risu-auth"], "account-secret");
        assert!(requests[0].body.is_empty());
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/api/account/write");
        assert_eq!(requests[1].body, body);
        assert_eq!(
            requests[1].headers["content-length"],
            body.len().to_string()
        );
        assert_eq!(
            requests[1].headers["content-type"],
            "application/octet-stream"
        );
        assert_eq!(requests[1].headers["x-risu-key"], "database/database.bin");
        assert_eq!(requests[1].headers["x-format"], "nocheck");
        assert_eq!(requests[1].headers["x-risu-session"], "42");
        assert_eq!(requests[1].headers["x-risu-save-date"], "1725000000123");
        assert_eq!(requests[1].headers["x-risu-auth"], "account-secret");
    }

    #[test]
    fn preserves_session_on_status_results_and_surfaces_json_metadata() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("snapshot.risudat");
        std::fs::write(&source, b"database").unwrap();
        let cases = [
            (
                MockResponse {
                    status: 304,
                    headers: &[],
                    body: b"",
                },
                OfficialPublicationUploadResult::NotModified {
                    replacement_key: "database/database.bin".to_owned(),
                    session: "existing".to_owned(),
                    save_date: "1725000000123".to_owned(),
                    status: 304,
                    bytes_uploaded: 8,
                },
            ),
            (
                MockResponse {
                    status: 403,
                    headers: &[("x-risu-status", "warn")],
                    body: b"ignored",
                },
                OfficialPublicationUploadResult::AuthWarning {
                    session: "existing".to_owned(),
                    save_date: "1725000000123".to_owned(),
                    status: 403,
                    bytes_uploaded: 8,
                    warning: None,
                },
            ),
            (
                MockResponse {
                    status: 403,
                    headers: &[],
                    body: b"login",
                },
                OfficialPublicationUploadResult::ReauthenticationNeeded {
                    session: "existing".to_owned(),
                    save_date: "1725000000123".to_owned(),
                    status: 403,
                    bytes_uploaded: 8,
                    warning: None,
                },
            ),
            (
                MockResponse {
                    status: 200,
                    headers: &[("content-type", "application/json; charset=utf-8")],
                    body: br#"{"warning":"quota","reloadSession":true}"#,
                },
                OfficialPublicationUploadResult::Written {
                    replacement_key: r#"{"warning":"quota","reloadSession":true}"#.to_owned(),
                    session: "existing".to_owned(),
                    save_date: "1725000000123".to_owned(),
                    status: 200,
                    bytes_uploaded: 8,
                    warning: Some("quota".to_owned()),
                    reload_session: true,
                },
            ),
        ];

        for (response, expected) in cases {
            let (base_url, requests, server) = mock_server(vec![response]);
            let mut input = request(base_url, &source);
            input.session = Some("existing".to_owned());
            let result = tauri::async_runtime::block_on(upload_file_attempt(input)).unwrap();
            server.join().unwrap();
            assert_eq!(result, expected);
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn uploads_the_prevalidated_open_handle_without_reopening_the_request_path() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("validated.risudat");
        let body = b"prevalidated export handle";
        std::fs::write(&source, body).unwrap();
        let source_file = std::fs::File::open(&source).unwrap();
        let (base_url, requests, server) = mock_server(vec![MockResponse {
            status: 200,
            headers: &[("content-type", "text/plain")],
            body: b"database/database.bin",
        }]);
        let mut input = request(base_url, &directory.path().join("missing.risudat"));
        input.session = Some("existing".to_owned());

        let result = tauri::async_runtime::block_on(upload_open_file_attempt(
            input,
            source_file,
            body.len() as u64,
        ))
        .unwrap();
        server.join().unwrap();

        assert!(matches!(
            result,
            OfficialPublicationUploadResult::Written { .. }
        ));
        assert_eq!(requests.lock().unwrap()[0].body, body);
    }

    #[test]
    fn authenticated_upload_does_not_follow_redirects() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("snapshot.risudat");
        std::fs::write(&source, b"database").unwrap();
        let (base_url, requests, server) = mock_server(vec![MockResponse {
            status: 302,
            headers: &[("location", "/redirected")],
            body: b"",
        }]);

        let mut input = request(base_url, &source);
        input.session = Some("existing".to_owned());
        let error = tauri::async_runtime::block_on(upload_file_attempt(input)).unwrap_err();
        server.join().unwrap();

        assert!(matches!(
            error,
            OfficialPublicationUploadError::HttpStatus { status: 302, .. }
        ));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/api/account/write");
    }

    #[test]
    fn serializes_the_tauri_result_with_the_typescript_field_names() {
        let result = OfficialPublicationUploadResult::Written {
            replacement_key: DATABASE_KEY.to_owned(),
            session: "42".to_owned(),
            save_date: "1725000000123".to_owned(),
            status: 200,
            bytes_uploaded: 21,
            warning: None,
            reload_session: false,
        };

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "kind": "written",
                "replacementKey": "database/database.bin",
                "session": "42",
                "saveDate": "1725000000123",
                "status": 200,
                "bytesUploaded": 21,
                "warning": null,
                "reloadSession": false,
            }),
        );
    }

    #[test]
    fn reauthentication_retry_reuses_the_session_and_restreams_the_exact_file() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("snapshot.risudat");
        let body = b"retry the same native file";
        std::fs::write(&source, body).unwrap();
        let (base_url, requests, server) = mock_server(vec![
            MockResponse {
                status: 200,
                headers: &[("content-type", "application/json")],
                body: br#"{"sessionNumber":"session-7"}"#,
            },
            MockResponse {
                status: 403,
                headers: &[],
                body: b"login",
            },
            MockResponse {
                status: 200,
                headers: &[("content-type", "text/plain")],
                body: b"database/database.bin",
            },
        ]);

        let first =
            tauri::async_runtime::block_on(upload_file_attempt(request(base_url.clone(), &source)))
                .unwrap();
        let session = match first {
            OfficialPublicationUploadResult::ReauthenticationNeeded { session, .. } => session,
            other => panic!("expected reauthentication, got {other:?}"),
        };
        let mut second = request(base_url, &source);
        second.session = Some(session);
        second.save_date = "1725000000456".to_owned();
        second.credential = OfficialPublicationCredential::RisuAuth {
            token: "refreshed-secret".to_owned(),
        };
        let result = tauri::async_runtime::block_on(upload_file_attempt(second)).unwrap();
        server.join().unwrap();

        assert!(matches!(
            result,
            OfficialPublicationUploadResult::Written { .. }
        ));
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.method.as_str(), request.path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("GET", "/api/account/getsessionnumber"),
                ("POST", "/api/account/write"),
                ("POST", "/api/account/write"),
            ],
        );
        assert_eq!(requests[1].body, body);
        assert_eq!(requests[2].body, body);
        assert_eq!(requests[1].headers["x-risu-session"], "session-7");
        assert_eq!(requests[2].headers["x-risu-session"], "session-7");
        assert_eq!(requests[1].headers["x-risu-auth"], "account-secret");
        assert_eq!(requests[2].headers["x-risu-auth"], "refreshed-secret");
        assert_eq!(requests[1].headers["x-risu-save-date"], "1725000000123");
        assert_eq!(requests[2].headers["x-risu-save-date"], "1725000000456");
    }

    #[test]
    fn classifies_body_independent_statuses_without_draining_large_bodies() {
        static OVERSIZED_BODY: [u8; MAX_RESPONSE_BYTES + 1] = [b'x'; MAX_RESPONSE_BYTES + 1];
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("snapshot.risudat");
        std::fs::write(&source, b"database").unwrap();

        for (status, headers, expected_kind) in [
            (304, &[][..], "not-modified"),
            (403, &[][..], "reauthentication-needed"),
            (403, &[("x-risu-status", "warn")][..], "auth-warning"),
        ] {
            let (base_url, _requests, server) = mock_server(vec![MockResponse {
                status,
                headers,
                body: &OVERSIZED_BODY,
            }]);
            let mut input = request(base_url, &source);
            input.session = Some("existing".to_owned());

            let result = tauri::async_runtime::block_on(upload_file_attempt(input)).unwrap();
            server.join().unwrap();

            let actual_kind = match result {
                OfficialPublicationUploadResult::NotModified { .. } => "not-modified",
                OfficialPublicationUploadResult::ReauthenticationNeeded { .. } => {
                    "reauthentication-needed"
                }
                OfficialPublicationUploadResult::AuthWarning { .. } => "auth-warning",
                OfficialPublicationUploadResult::Written { .. } => "written",
            };
            assert_eq!(actual_kind, expected_kind);
        }
    }

    #[test]
    fn forbidden_json_keeps_warning_text_on_both_authentication_outcomes() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("snapshot.risudat");
        std::fs::write(&source, b"snapshot").unwrap();
        for warn in [false, true] {
            let headers: &[(&str, &str)] = if warn {
                &[
                    ("content-type", "application/json"),
                    ("x-risu-status", "warn"),
                ]
            } else {
                &[("content-type", "application/json")]
            };
            let (base_url, _, server) = mock_server(vec![MockResponse {
                status: 403,
                headers,
                body: br#"{"warning":"quota","reloadSession":true}"#,
            }]);
            let mut input = request(base_url, &source);
            input.session = Some("existing".to_owned());
            let result = tauri::async_runtime::block_on(upload_file_attempt(input)).unwrap();
            server.join().unwrap();
            let value = serde_json::to_value(result).unwrap();
            assert_eq!(value["warning"], "quota");
            assert_eq!(
                value["kind"],
                if warn {
                    "auth-warning"
                } else {
                    "reauthentication-needed"
                }
            );
            assert!(value.get("reloadSession").is_none());
        }
    }
}
