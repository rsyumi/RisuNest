use super::*;
use crate::external_storage::{
    auth::PendingAuthorization,
    fake::{loopback_dependencies, with_transport, MemoryVault, TestDependencies},
    transfer::{SpoolSink, SpoolSource},
    wire_fixture::{Reply, WireRequest, WireServer},
};
use std::sync::{atomic::Ordering, Mutex};

const SECRET: &str = "onedrive-secret";
const NOW_MS: u64 = 1_700_000_000_000;
const ROOT_ITEM: &str = "root-item";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn token_document() -> Vec<u8> {
    format!(
        "{{\"refreshToken\":\"synthetic-refresh\",\"accessToken\":\"synthetic-access\",\"accessTokenExpiresAtMs\":{}}}",
        NOW_MS + 3_600_000
    )
    .into_bytes()
}

fn harness(now_ms: u64) -> TestDependencies {
    loopback_dependencies(MemoryVault::with(SECRET, &token_document()), now_ms)
}

fn default_tenant(account_type: &str) -> &'static str {
    match account_type {
        "business" => "organizations",
        _ => "consumers",
    }
}

fn config_at(endpoint: &str, account_type: &str) -> ConnectionConfig {
    let mut location = BTreeMap::new();
    location.insert("accountType".to_owned(), account_type.to_owned());
    location.insert("tenant".to_owned(), default_tenant(account_type).to_owned());
    location.insert("driveId".to_owned(), "drive-1".to_owned());
    location.insert(
        "rootItemId".to_owned(),
        if account_type == "appFolder" {
            config::APP_ROOT.to_owned()
        } else {
            "configured-root".to_owned()
        },
    );
    ConnectionConfig {
        provider: "onedrive".to_owned(),
        profile: Some(account_type.to_owned()),
        endpoint: endpoint.to_owned(),
        account_id: "account-1".to_owned(),
        location,
        oauth_profile: Some(OAuthProfile {
            project_id: "entra-application".to_owned(),
            platform_client_ids: BTreeMap::new(),
        }),
    }
}

fn config_for(server: &WireServer, account_type: &str) -> ConnectionConfig {
    config_at(server.url.as_str(), account_type)
}

fn secret() -> SecretRef {
    SecretRef(SECRET.to_owned())
}

fn json(status: u16, body: &str) -> Reply {
    Reply::Http {
        status,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

fn empty(status: u16, headers: Vec<(&str, &str)>) -> Reply {
    Reply::Http {
        status,
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        body: Vec::new(),
    }
}

fn root_folder() -> String {
    format!("{{\"id\":\"{ROOT_ITEM}\",\"name\":\"RisuNest\",\"folder\":{{\"childCount\":5}}}}")
}

fn file_item(name: &str, size: u64, etag: &str) -> String {
    format!(
        "{{\"id\":\"item-{name}\",\"name\":\"{name}\",\"size\":{size},\"eTag\":\"{etag}\",\"file\":{{}}}}"
    )
}

fn descriptor_page() -> String {
    format!("{{\"value\":[{}]}}", file_item("root.rnd", 12, "etag-d"))
}

/// The two replies every `OpenMode::Existing` connection consumes.
fn existing_open() -> Vec<Reply> {
    vec![json(200, &root_folder()), json(200, &descriptor_page())]
}

fn created_folders() -> Vec<Reply> {
    let mut replies = vec![json(200, &root_folder())];
    for folder in config::REPOSITORY_FOLDERS {
        replies.push(json(
            201,
            &format!("{{\"id\":\"id-{folder}\",\"name\":\"{folder}\",\"folder\":{{}}}}"),
        ));
    }
    replies
}

fn head(request: &WireRequest) -> String {
    request.headers.to_lowercase()
}

fn line(request: &WireRequest) -> String {
    request
        .headers
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn locator_for(repository: &RepositoryHandle, object: &str) -> RemoteLocator {
    RemoteLocator {
        connection_identity: repository.connection_identity.clone(),
        collection: None,
        object: object.to_owned(),
    }
}

fn intent_for(
    repository: &RepositoryHandle,
    object_id: &str,
    role: ObjectRole,
    bytes: &[u8],
) -> ObjectIntent {
    ObjectIntent {
        repository_id: repository.repository_id.clone(),
        job_id: "job-1".to_owned(),
        object_id: object_id.to_owned(),
        role,
        byte_length: bytes.len() as u64,
        sha256: risunest_sync_wire::hash(bytes),
    }
}

fn source_of(directory: &std::path::Path, name: &str, bytes: &[u8]) -> SpoolSource {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    SpoolSource::verified(&path, bytes.len() as u64, &risunest_sync_wire::hash(bytes)).unwrap()
}

/// Repository handles and resolutions carry no `Debug`, so failures are read
/// out by pattern instead of `unwrap_err`.
fn failed<T>(result: Result<T>) -> ProviderError {
    match result {
        Ok(_) => panic!("expected a provider error"),
        Err(error) => error,
    }
}

async fn open(
    provider: &Arc<dyn Provider>,
    config: &ConnectionConfig,
    mode: OpenMode,
    cancel: &Cancellation,
) -> Result<(RepositoryHandle, Capabilities)> {
    provider
        .open_repository(config, &secret(), mode, cancel)
        .await
}

/// Injected transport for cases the loopback fixture cannot express: request
/// bodies above its 4 MiB cap and absolute service URLs that must match the
/// configured endpoint origin.
struct Exchange {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

struct Sent {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    body_length: u64,
}

#[derive(Default)]
struct ScriptedTransport {
    replies: Mutex<std::collections::VecDeque<Exchange>>,
    sent: Mutex<Vec<Sent>>,
}

impl ScriptedTransport {
    fn with(replies: Vec<Exchange>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            sent: Mutex::new(Vec::new()),
        })
    }
}

impl crate::external_storage::http::HttpTransport for ScriptedTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HttpResponse> {
        Box::pin(async move {
            use tokio::io::AsyncReadExt;
            cancel.check()?;
            let method = request.method.to_string();
            let url = request.url.to_string();
            let headers = request.headers.clone();
            let declared = request.content_length;
            let mut body_length = 0u64;
            if let Some(mut body) = request.body {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    let read = body
                        .read(&mut buffer)
                        .await
                        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
                    if read == 0 {
                        break;
                    }
                    body_length += read as u64;
                }
                assert_eq!(declared, Some(body_length), "declared content length");
            }
            self.sent.lock().unwrap().push(Sent {
                method,
                url,
                headers,
                body_length,
            });
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ProviderError::new(ErrorKind::Transient))?;
            let mut headers: BTreeMap<String, String> = reply
                .headers
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect();
            headers.insert("content-length".to_owned(), reply.body.len().to_string());
            Ok(HttpResponse {
                status: reply.status,
                headers,
                body: Box::pin(std::io::Cursor::new(reply.body)),
            })
        })
    }
}

fn scripted(replies: Vec<Exchange>) -> (Arc<ScriptedTransport>, TestDependencies) {
    let transport = ScriptedTransport::with(replies);
    let harness = with_transport(
        transport.clone(),
        MemoryVault::with(SECRET, &token_document()),
        NOW_MS,
    );
    (transport, harness)
}

fn exchange(status: u16, body: &str) -> Exchange {
    Exchange {
        status,
        headers: vec![("content-type", "application/json".to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

#[test]
fn configuration_validation_rejects_foreign_and_incomplete_connections() {
    let graph = "https://graph.microsoft.com/v1.0";
    assert!(config::validate(&config_at(graph, "personal")).is_ok());
    assert!(config::validate(&config_at(graph, "business")).is_ok());
    assert!(config::validate(&config_at(graph, "appFolder")).is_ok());

    let mut foreign = config_at(graph, "personal");
    foreign.provider = "google_drive".to_owned();
    assert_eq!(
        failed(config::validate(&foreign)).kind,
        ErrorKind::Unsupported
    );

    let plain = config_at("http://graph.microsoft.com/v1.0", "personal");
    assert_eq!(
        failed(config::validate(&plain)).kind,
        ErrorKind::Unsupported
    );
    // The wire fixture is the only plain-http host the adapter accepts.
    assert!(config::validate(&config_at("http://127.0.0.1:9/synthetic", "personal")).is_ok());

    for key in ["driveId", "rootItemId", "tenant", "accountType"] {
        let mut missing = config_at(graph, "personal");
        missing.location.remove(key);
        assert_eq!(
            failed(config::validate(&missing)).kind,
            ErrorKind::Unsupported,
            "{key}"
        );
    }
    let mut unknown = config_at(graph, "personal");
    unknown
        .location
        .insert("uploadUrl".to_owned(), "https://elsewhere".to_owned());
    assert!(config::validate(&unknown).is_err());

    let mut no_app = config_at(graph, "personal");
    no_app.oauth_profile = None;
    assert!(config::validate(&no_app).is_err());

    // A personal account never signs in through an organization endpoint.
    let mut wrong_tenant = config_at(graph, "personal");
    wrong_tenant
        .location
        .insert("tenant".to_owned(), "organizations".to_owned());
    assert!(config::validate(&wrong_tenant).is_err());

    // The app folder is reached through its alias, not an arbitrary item.
    let mut wrong_root = config_at(graph, "appFolder");
    wrong_root
        .location
        .insert("rootItemId".to_owned(), "some-item".to_owned());
    assert!(config::validate(&wrong_root).is_err());

    let mut mismatched_profile = config_at(graph, "personal");
    mismatched_profile.profile = Some("business".to_owned());
    assert!(config::validate(&mismatched_profile).is_err());

    // Names OneDrive cannot store never become a locator.
    for rejected in [
        "packs/a:b",
        "packs/a|b",
        "packs/a#b",
        "packs/a%b",
        "packs/..",
        "packs/~a",
        "packs/a.",
        "packs//a",
        "",
    ] {
        assert!(
            config::validate_relative_path(rejected).is_err(),
            "{rejected}"
        );
    }
    assert!(config::validate_relative_path("packs/0a1b2c").is_ok());

    // Two devices compute the same identity from the same configuration.
    assert_eq!(
        config::validate(&config_at(graph, "personal"))
            .unwrap()
            .identity,
        config::validate(&config_at(graph, "personal"))
            .unwrap()
            .identity
    );
    assert_ne!(
        config::validate(&config_at(graph, "personal"))
            .unwrap()
            .identity,
        config::validate(&config_at(graph, "appFolder"))
            .unwrap()
            .identity
    );
}

#[test]
fn a_missing_vault_secret_requires_reauthentication_before_any_request() {
    runtime().block_on(async {
        let server = WireServer::start(Vec::new());
        let harness = with_transport(
            Arc::new(crate::external_storage::http::NativeHttpTransport::for_loopback_tests()),
            MemoryVault::default(),
            NOW_MS,
        );
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let error = failed(
            open(
                &provider,
                &config_for(&server, "personal"),
                OpenMode::Existing,
                &cancel,
            )
            .await,
        );
        assert_eq!(error.kind, ErrorKind::ReauthRequired);
        assert!(server.requests.lock().unwrap().is_empty());
    });
}

#[test]
fn a_denied_budget_sends_no_request() {
    runtime().block_on(async {
        let server = WireServer::start(existing_open());
        let harness = harness(NOW_MS);
        harness.budget.deny.store(true, Ordering::SeqCst);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let error = failed(
            open(
                &provider,
                &config_for(&server, "personal"),
                OpenMode::Existing,
                &cancel,
            )
            .await,
        );
        assert_eq!(error.kind, ErrorKind::DailyQuotaExhausted);
        assert!(server.requests.lock().unwrap().is_empty());
        assert_eq!(harness.budget.reservations.lock().unwrap().len(), 1);
    });
}

#[test]
fn opening_an_existing_repository_needs_a_descriptor_and_creates_nothing() {
    runtime().block_on(async {
        let server = WireServer::start(existing_open());
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, capabilities) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(repository.repository_id, repository.connection_identity);
        assert!(capabilities.require(PublicationStrategy::Sequential).is_ok());
        // `if-match` on a content PUT is undocumented, so CAS stays refused.
        assert!(capabilities.require(PublicationStrategy::Cas).is_err());
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert!(line(&records[0]).starts_with(
            "GET /synthetic/drives/drive-1/items/configured-root?$select=id,name,size,eTag,file,folder"
        ));
        assert!(line(&records[1]).starts_with(&format!(
            "GET /synthetic/drives/drive-1/items/{ROOT_ITEM}:/descriptors:/children?$top=1"
        )));
        assert!(head(&records[1]).contains("authorization: bearer synthetic-access"));
        assert!(records.iter().all(|record| !line(record).starts_with("POST")
            && !line(record).starts_with("PUT")));
    });
}

#[test]
fn an_existing_open_reports_not_found_for_another_account_and_an_empty_root() {
    runtime().block_on(async {
        // The descriptor folder does not exist in this drive.
        let absent = WireServer::start(vec![json(200, &root_folder()), json(404, "{}")]);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let error = failed(
            open(
                &provider,
                &config_for(&absent, "personal"),
                OpenMode::Existing,
                &cancel,
            )
            .await,
        );
        assert_eq!(error.kind, ErrorKind::NotFound);
        assert_eq!(absent.requests.lock().unwrap().len(), 2);

        // The folder exists but holds no descriptor.
        let bare = WireServer::start(vec![json(200, &root_folder()), json(200, "{\"value\":[]}")]);
        let error = failed(
            open(
                &provider,
                &config_for(&bare, "personal"),
                OpenMode::Existing,
                &cancel,
            )
            .await,
        );
        assert_eq!(error.kind, ErrorKind::NotFound);

        // A drive that the signed in account cannot reach at all.
        let foreign = WireServer::start(vec![json(404, "{}")]);
        let error = failed(
            open(
                &provider,
                &config_for(&foreign, "personal"),
                OpenMode::Existing,
                &cancel,
            )
            .await,
        );
        assert_eq!(error.kind, ErrorKind::NotFound);
        assert_eq!(foreign.requests.lock().unwrap().len(), 1);
    });
}

#[test]
fn creating_a_repository_provisions_every_folder_and_refuses_an_occupied_root() {
    runtime().block_on(async {
        let server = WireServer::start(created_folders());
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Create,
            &cancel,
        )
        .await
        .unwrap();
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 1 + config::REPOSITORY_FOLDERS.len());
        for (index, folder) in config::REPOSITORY_FOLDERS.iter().enumerate() {
            let record = &records[index + 1];
            assert_eq!(
                line(record),
                format!("POST /synthetic/drives/drive-1/items/{ROOT_ITEM}/children HTTP/1.1")
            );
            let body = String::from_utf8(record.body.clone()).unwrap();
            assert!(body.contains(&format!("\"name\":\"{folder}\"")));
            assert!(body.contains("\"@microsoft.graph.conflictBehavior\":\"fail\""));
        }
        drop(records);

        let occupied = WireServer::start(vec![
            json(200, &root_folder()),
            json(409, "{\"error\":{\"code\":\"nameAlreadyExists\"}}"),
        ]);
        let error = failed(
            open(
                &provider,
                &config_for(&occupied, "personal"),
                OpenMode::Create,
                &cancel,
            )
            .await,
        );
        assert_eq!(error.kind, ErrorKind::PreconditionFailed);
        assert_eq!(error.http_status, Some(409));
        // The first conflicting folder stops the whole provisioning.
        assert_eq!(occupied.requests.lock().unwrap().len(), 2);
    });
}

#[test]
fn each_account_type_resolves_its_own_root_and_reports_its_own_evidence() {
    runtime().block_on(async {
        for (account_type, expected_root) in [
            ("personal", "items/configured-root"),
            ("business", "items/configured-root"),
            ("appFolder", "special/approot"),
        ] {
            let server = WireServer::start(existing_open());
            let harness = harness(NOW_MS);
            let provider = create(harness.dependencies.clone()).unwrap();
            let cancel = Cancellation::default();
            let (repository, capabilities) = open(
                &provider,
                &config_for(&server, account_type),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .unwrap();
            assert!(repository.connection_identity.contains(account_type));
            assert_eq!(capabilities.documented_at.as_deref(), Some(DOCUMENTED_AT));
            assert_eq!(capabilities.upload_alignment, 320 * 1024);
            assert!(capabilities.resumable_upload && capabilities.conditional_get);
            assert_eq!(capabilities.max_stored_bytes, None);
            assert_eq!(capabilities.discovery_extra_requests, 0);
            let app_folder_evidence = capabilities
                .evidence_urls
                .iter()
                .any(|url| url.contains("onedrive-sharepoint-appfolder"));
            assert_eq!(app_folder_evidence, account_type == "appFolder");
            let records = server.requests.lock().unwrap();
            assert!(
                line(&records[0]).contains(&format!("/synthetic/drives/drive-1/{expected_root}")),
                "{account_type}: {}",
                line(&records[0])
            );
        }
    });
}

#[test]
fn reading_follows_the_download_redirect_to_another_origin_without_authorization() {
    runtime().block_on(async {
        let payload = vec![7u8; 5_000];
        let download = WireServer::start(vec![Reply::Http {
            status: 200,
            headers: vec![(
                "Content-Type".to_owned(),
                "application/octet-stream".to_owned(),
            )],
            body: payload.clone(),
        }]);
        let mut replies = existing_open();
        replies.push(Reply::Http {
            status: 302,
            headers: vec![
                ("Location".to_owned(), download.url.to_string()),
                ("ETag".to_owned(), "etag-pack".to_owned()),
            ],
            body: Vec::new(),
        });
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("received"), 5_000).unwrap();
        let locator = locator_for(&repository, "packs/pack-1");
        let receipt = provider
            .read_object(&repository, &locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        let ReadReceipt::Body(body) = receipt else {
            panic!("expected a body");
        };
        assert_eq!(body.byte_length, 5_000);
        assert_eq!(body.version, Some(VersionToken("etag-pack".to_owned())));
        let checksum = body.checksum.unwrap();
        assert_eq!(checksum.value, risunest_sync_wire::hash(&payload));
        assert!(!checksum.provider_verified);
        assert!(sink.is_verified());

        let graph_records = server.requests.lock().unwrap();
        assert_eq!(graph_records.len(), 3);
        assert!(line(&graph_records[2]).starts_with(&format!(
            "GET /synthetic/drives/drive-1/items/{ROOT_ITEM}:/packs/pack-1:/content"
        )));
        assert!(head(&graph_records[2]).contains("authorization: bearer"));
        let download_records = download.requests.lock().unwrap();
        assert_eq!(download_records.len(), 1);
        assert!(!head(&download_records[0]).contains("authorization"));

        // Issuing the URL and fetching the body reserve budget separately.
        let operations: Vec<_> = harness
            .budget
            .reservations
            .lock()
            .unwrap()
            .iter()
            .map(|(costs, _)| costs.len())
            .collect();
        assert_eq!(operations, vec![1, 1, 1, 1]);
    });
}

#[test]
fn an_unchanged_token_produces_a_conditional_not_modified_read() {
    runtime().block_on(async {
        let mut replies = existing_open();
        replies.push(empty(304, vec![("ETag", "etag-head")]));
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("unused"), 64).unwrap();
        let token = VersionToken("etag-head".to_owned());
        let locator = provider.head_locator(&repository).unwrap();
        let receipt = provider
            .read_object(&repository, &locator, Some(&token), &mut sink, &cancel)
            .await
            .unwrap();
        assert_eq!(receipt, ReadReceipt::NotModified(token));
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert!(head(&records[2]).contains("if-none-match: etag-head"));
    });
}

#[test]
fn cancellation_during_the_download_body_stops_the_read() {
    runtime().block_on(async {
        let download = WireServer::start(vec![Reply::DelayedBody]);
        let mut replies = existing_open();
        replies.push(Reply::Http {
            status: 302,
            headers: vec![("Location".to_owned(), download.url.to_string())],
            body: Vec::new(),
        });
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("partial"), 100).unwrap();
        let locator = locator_for(&repository, "packs/pack-1");
        let read = async {
            let error = provider
                .read_object(&repository, &locator, None, &mut sink, &cancel)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Cancelled);
        };
        let trigger = async {
            while download.requests.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel.cancel();
        };
        tokio::time::timeout(
            std::time::Duration::from_millis(2_000),
            futures::future::join(read, trigger),
        )
        .await
        .unwrap();
        assert!(!sink.is_verified());
    });
}

#[test]
fn an_immutable_create_uses_one_conditional_put_and_converges_after_a_lost_response() {
    runtime().block_on(async {
        let bytes = vec![3u8; 1_024];
        let mut replies = existing_open();
        replies.push(Reply::Lost);
        replies.push(json(409, "{\"error\":{\"code\":\"nameAlreadyExists\"}}"));
        replies.push(json(200, &file_item("pack-1", 1_024, "etag-1")));
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source_of(directory.path(), "pack", &bytes);
        let intent = intent_for(&repository, "pack-1", ObjectRole::Pack, &bytes);
        assert!(provider
            .begin_upload(&repository, &intent, &cancel)
            .await
            .unwrap()
            .is_none());
        // The response is lost, the object may or may not exist remotely.
        assert!(provider
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .is_err());
        // The retry finds the same bytes under the same identity.
        let receipt = provider
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.byte_length, 1_024);
        assert_eq!(receipt.locator.object, "packs/pack-1");
        assert_eq!(receipt.version, Some(VersionToken("etag-1".to_owned())));
        assert!(receipt.checksum.is_none());

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 5);
        for index in [2, 3] {
            assert!(line(&records[index]).starts_with(&format!(
                "PUT /synthetic/drives/drive-1/items/{ROOT_ITEM}:/packs/pack-1:/content?@microsoft.graph.conflictBehavior=fail"
            )));
            assert_eq!(records[index].body, bytes);
        }
        assert!(line(&records[4]).starts_with(&format!(
            "GET /synthetic/drives/drive-1/items/{ROOT_ITEM}:/packs/pack-1?$select="
        )));
    });
}

#[test]
fn a_stored_object_of_another_length_is_a_precondition_failure() {
    runtime().block_on(async {
        let bytes = vec![3u8; 1_024];
        let mut replies = existing_open();
        replies.push(json(409, "{\"error\":{\"code\":\"nameAlreadyExists\"}}"));
        replies.push(json(200, &file_item("pack-1", 999, "etag-other")));
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source_of(directory.path(), "pack", &bytes);
        let intent = intent_for(&repository, "pack-1", ObjectRole::Pack, &bytes);
        let error = provider
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::PreconditionFailed);
        assert_eq!(server.requests.lock().unwrap().len(), 4);

        // A source that disagrees with the intent never reaches the network.
        let shorter = source_of(directory.path(), "shorter", &bytes[..10]);
        assert_eq!(
            provider
                .create_object(&repository, &intent, &shorter, None, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(server.requests.lock().unwrap().len(), 4);
    });
}

#[test]
fn a_session_opens_only_above_the_simple_upload_gate_and_seals_its_url() {
    runtime().block_on(async {
        let upload = WireServer::start(Vec::new());
        let upload_url = upload.url.to_string();
        let mut replies = existing_open();
        replies.push(json(
            200,
            &format!(
                "{{\"uploadUrl\":\"{upload_url}\",\"expirationDateTime\":\"2026-09-15T00:00:00Z\"}}"
            ),
        ));
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let small = ObjectIntent {
            byte_length: graph::SIMPLE_UPLOAD_MAX_BYTES,
            ..intent_for(&repository, "pack-small", ObjectRole::Pack, b"small")
        };
        assert!(provider
            .begin_upload(&repository, &small, &cancel)
            .await
            .unwrap()
            .is_none());
        assert_eq!(server.requests.lock().unwrap().len(), 2);

        let large = ObjectIntent {
            byte_length: graph::SIMPLE_UPLOAD_MAX_BYTES + 1,
            ..intent_for(&repository, "pack-large", ObjectRole::Pack, b"large")
        };
        let state = provider
            .begin_upload(&repository, &large, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.confirmed_offset, 0);
        assert_eq!(state.expires_at_ms, Some(1_789_430_400_000));
        // The upload URL only lives inside the vault, never in the locator.
        let sealed = harness.vault.contents(&state.sealed_state.0).unwrap();
        let sealed = String::from_utf8(sealed).unwrap();
        assert!(sealed.contains(&upload_url) && sealed.contains("pack-large"));

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert!(line(&records[2]).starts_with(&format!(
            "POST /synthetic/drives/drive-1/items/{ROOT_ITEM}:/packs/pack-large:/createUploadSession"
        )));
        let body = String::from_utf8(records[2].body.clone()).unwrap();
        assert!(body.contains("\"@microsoft.graph.conflictBehavior\":\"fail\""));
        assert!(body.contains("\"name\":\"pack-large\""));
        assert!(body.contains("\"deferCommit\":false"));
    });
}

#[test]
fn session_fragments_are_320_kib_aligned_and_sent_in_order_without_authorization() {
    runtime().block_on(async {
        let total = graph::FRAGMENT_BYTES + graph::FRAGMENT_ALIGNMENT;
        let (transport, harness) = scripted(vec![
            exchange(200, &root_folder()),
            exchange(200, &descriptor_page()),
            exchange(
                202,
                &format!(
                    "{{\"expirationDateTime\":\"2026-09-15T00:00:00Z\",\"nextExpectedRanges\":[\"{}-\"]}}",
                    graph::FRAGMENT_BYTES
                ),
            ),
            exchange(201, &file_item("pack-big", total, "etag-big")),
        ]);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let config = config_at("https://graph.microsoft.com/v1.0", "business");
        let (repository, _) = open(&provider, &config, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
        let source = source_of(directory.path(), "big", &bytes);
        let intent = intent_for(&repository, "pack-big", ObjectRole::Pack, &bytes);
        let sealed = tokens::encode_session(&tokens::SealedSession {
            upload_url: zeroize::Zeroizing::new("https://sn3302.up.1drv.com/up/session".to_owned()),
            object_id: intent.object_id.clone(),
            byte_length: total,
        })
        .unwrap();
        let resume = ResumeState {
            sealed_state: harness.dependencies.vault.store(&sealed).await.unwrap(),
            confirmed_offset: 0,
            expires_at_ms: None,
        };
        let receipt = provider
            .create_object(&repository, &intent, &source, Some(&resume), &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.byte_length, total);

        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent.len(), 4);
        let fragments = &sent[2..];
        assert_eq!(fragments[0].body_length, graph::FRAGMENT_BYTES);
        assert_eq!(fragments[1].body_length, graph::FRAGMENT_ALIGNMENT);
        for fragment in fragments {
            assert_eq!(fragment.method, "PUT");
            assert_eq!(fragment.url, "https://sn3302.up.1drv.com/up/session");
            assert!(!fragment.headers.contains_key("authorization"));
            assert_eq!(fragment.body_length % graph::FRAGMENT_ALIGNMENT, 0);
        }
        assert_eq!(
            fragments[0].headers["content-range"],
            format!("bytes 0-{}/{total}", graph::FRAGMENT_BYTES - 1)
        );
        assert_eq!(
            fragments[1].headers["content-range"],
            format!("bytes {}-{}/{total}", graph::FRAGMENT_BYTES, total - 1)
        );
    });
}

#[test]
fn reconciliation_reports_the_first_missing_range_as_the_confirmed_offset() {
    runtime().block_on(async {
        let upload = WireServer::start(vec![json(
            200,
            "{\"expirationDateTime\":\"2026-09-15T00:00:00Z\",\"nextExpectedRanges\":[\"5000-9999\",\"1000-2000\"]}",
        )]);
        let upload_url = upload.url.to_string();
        let server = WireServer::start(existing_open());
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let intent = ObjectIntent {
            byte_length: 20_000,
            ..intent_for(&repository, "pack-gap", ObjectRole::Pack, b"gap")
        };
        let sealed = tokens::encode_session(&tokens::SealedSession {
            upload_url: zeroize::Zeroizing::new(upload_url),
            object_id: intent.object_id.clone(),
            byte_length: 20_000,
        })
        .unwrap();
        let resume = ResumeState {
            sealed_state: harness.dependencies.vault.store(&sealed).await.unwrap(),
            confirmed_offset: 0,
            expires_at_ms: None,
        };
        let resolution = provider
            .reconcile_upload(&repository, &intent, &resume, &cancel)
            .await
            .unwrap();
        let UploadResolution::Resumable(updated) = resolution else {
            panic!("expected a resumable session");
        };
        assert_eq!(updated.confirmed_offset, 1_000);
        assert_eq!(updated.expires_at_ms, Some(1_789_430_400_000));
        let records = upload.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert!(line(&records[0]).starts_with("GET /synthetic"));
        assert!(!head(&records[0]).contains("authorization"));

        // A sealed session bound to another object never resumes.
        let foreign = ObjectIntent {
            object_id: "pack-other".to_owned(),
            ..intent
        };
        assert_eq!(
            failed(
                provider
                    .reconcile_upload(&repository, &foreign, &resume, &cancel)
                    .await
            )
            .kind,
            ErrorKind::Corrupt
        );
    });
}

#[test]
fn an_expired_session_restarts_completes_or_conflicts_on_the_remote_truth() {
    runtime().block_on(async {
        for (item, expected) in [
            (None, "restart"),
            (Some(file_item("pack-x", 2_048, "etag-x")), "complete"),
            (Some(file_item("pack-x", 99, "etag-x")), "conflict"),
        ] {
            let upload =
                WireServer::start(vec![json(404, "{\"error\":{\"code\":\"itemNotFound\"}}")]);
            let upload_url = upload.url.to_string();
            let mut replies = existing_open();
            replies.push(match &item {
                Some(body) => json(200, body),
                None => json(404, "{\"error\":{\"code\":\"itemNotFound\"}}"),
            });
            let server = WireServer::start(replies);
            let harness = harness(NOW_MS);
            let provider = create(harness.dependencies.clone()).unwrap();
            let cancel = Cancellation::default();
            let (repository, _) = open(
                &provider,
                &config_for(&server, "personal"),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .unwrap();
            let intent = ObjectIntent {
                byte_length: 2_048,
                ..intent_for(&repository, "pack-x", ObjectRole::Pack, b"x")
            };
            let sealed = tokens::encode_session(&tokens::SealedSession {
                upload_url: zeroize::Zeroizing::new(upload_url),
                object_id: intent.object_id.clone(),
                byte_length: 2_048,
            })
            .unwrap();
            let resume = ResumeState {
                sealed_state: harness.dependencies.vault.store(&sealed).await.unwrap(),
                confirmed_offset: 0,
                expires_at_ms: None,
            };
            let resolution = provider
                .reconcile_upload(&repository, &intent, &resume, &cancel)
                .await
                .unwrap();
            let observed = match resolution {
                UploadResolution::RestartRequired => "restart",
                UploadResolution::Complete(receipt) => {
                    assert!(receipt.complete);
                    assert_eq!(receipt.locator.object, "packs/pack-x");
                    "complete"
                }
                UploadResolution::Conflict => "conflict",
                UploadResolution::Resumable(_) => "resumable",
            };
            assert_eq!(observed, expected);
        }
    });
}

#[test]
fn head_writes_send_exactly_one_request_and_never_retry_a_lost_response() {
    runtime().block_on(async {
        let mut replies = existing_open();
        replies.push(Reply::Lost);
        replies.push(json(201, &file_item("head", 4, "etag-h2")));
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let locator = provider.head_locator(&repository).unwrap();
        let head_bytes = HeadBytes::new(b"head".to_vec()).unwrap();
        assert!(provider
            .replace_head(&repository, &locator, &head_bytes, &cancel)
            .await
            .is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 3);
        let receipt = provider
            .replace_head(&repository, &locator, &head_bytes, &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.version, Some(VersionToken("etag-h2".to_owned())));
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 4);
        for record in &records[2..] {
            assert_eq!(
                line(record),
                format!("PUT /synthetic/drives/drive-1/items/{ROOT_ITEM}:/head:/content HTTP/1.1")
            );
            assert_eq!(record.body, b"head");
            assert!(!head(record).contains("if-match"));
        }
    });
}

#[test]
fn head_preconditions_use_create_if_absent_and_an_exact_version() {
    runtime().block_on(async {
        let mut replies = existing_open();
        replies.push(json(201, &file_item("head", 4, "etag-new")));
        replies.push(json(409, "{\"error\":{\"code\":\"nameAlreadyExists\"}}"));
        replies.push(json(200, &file_item("head", 4, "etag-next")));
        replies.push(json(412, "{\"error\":{\"code\":\"resourceModified\"}}"));
        let server = WireServer::start(replies);
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let locator = provider.head_locator(&repository).unwrap();
        let head_bytes = HeadBytes::new(b"head".to_vec()).unwrap();

        let receipt = provider
            .compare_exchange_head(
                &repository,
                &locator,
                &ExpectedHead::Absent,
                &head_bytes,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(receipt.version, Some(VersionToken("etag-new".to_owned())));

        let conflict = provider
            .compare_exchange_head(
                &repository,
                &locator,
                &ExpectedHead::Absent,
                &head_bytes,
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(conflict.kind, ErrorKind::PreconditionFailed);
        assert_eq!(conflict.http_status, Some(409));

        let exact = ExpectedHead::Exact(VersionToken("etag-new".to_owned()));
        let updated = provider
            .compare_exchange_head(&repository, &locator, &exact, &head_bytes, &cancel)
            .await
            .unwrap();
        assert_eq!(updated.version, Some(VersionToken("etag-next".to_owned())));

        let stale = ExpectedHead::Exact(VersionToken("etag-stale".to_owned()));
        let rejected = provider
            .compare_exchange_head(&repository, &locator, &stale, &head_bytes, &cancel)
            .await
            .unwrap_err();
        assert_eq!(rejected.kind, ErrorKind::PreconditionFailed);
        assert_eq!(rejected.http_status, Some(412));

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 6);
        for record in &records[2..4] {
            assert!(line(record).contains("?@microsoft.graph.conflictBehavior=fail"));
            assert!(!head(record).contains("if-match"));
        }
        for record in &records[4..] {
            assert!(!line(record).contains("conflictBehavior"));
        }
        assert!(head(&records[4]).contains("if-match: etag-new"));
        assert!(head(&records[5]).contains("if-match: etag-stale"));
    });
}

#[test]
fn listing_pages_with_a_skip_token_and_bounds_the_limit() {
    runtime().block_on(async {
        let endpoint = "https://graph.microsoft.com/v1.0";
        let (transport, harness) = scripted(vec![
            exchange(200, &root_folder()),
            exchange(200, &descriptor_page()),
            exchange(
                200,
                &format!(
                    "{{\"value\":[{},{},{{\"id\":\"f\",\"name\":\"stray\",\"folder\":{{}}}},{{\"id\":\"b\",\"name\":\"bad:name\",\"size\":1,\"file\":{{}}}}],\"@odata.nextLink\":\"{endpoint}/drives/drive-1/items/{ROOT_ITEM}:/snapshots:/children?$top=2&$skiptoken=PAGE2\"}}",
                    file_item("snap-a", 10, "etag-a"),
                    file_item("snap-b", 20, "etag-b")
                ),
            ),
            exchange(200, &format!("{{\"value\":[{}]}}", file_item("snap-c", 30, "etag-c"))),
        ]);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_at(endpoint, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();

        for limit in [0u16, 1001] {
            assert_eq!(
                provider
                    .list_objects(&repository, Collection::Snapshots, None, limit, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }

        let page = provider
            .list_objects(&repository, Collection::Snapshots, None, 2, &cancel)
            .await
            .unwrap();
        // Folders and names this adapter could not have written are skipped.
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].locator.object, "snapshots/snap-a");
        assert_eq!(page.objects[0].byte_length, 10);
        assert_eq!(
            page.objects[1].version,
            Some(VersionToken("etag-b".to_owned()))
        );
        assert_eq!(page.next_cursor.as_deref(), Some("PAGE2"));

        let last = provider
            .list_objects(
                &repository,
                Collection::BackupPoints,
                page.next_cursor.as_deref(),
                2,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(last.objects.len(), 1);
        assert_eq!(last.objects[0].locator.object, "points/snap-c");
        assert!(last.next_cursor.is_none());

        let sent = transport.sent.lock().unwrap();
        assert!(sent[2]
            .url
            .contains(&format!("items/{ROOT_ITEM}:/snapshots:/children?$top=2")));
        assert!(sent[3].url.contains("items/root-item:/points:/children"));
        assert!(sent[3].url.contains("$skiptoken=PAGE2"));
    });
}

#[test]
fn throttling_storage_and_bandwidth_statuses_carry_their_documented_meaning() {
    runtime().block_on(async {
        let cases = [
            (
                429u16,
                Some("30"),
                ErrorKind::RateLimited,
                Some(NOW_MS + 30_000),
            ),
            (507, None, ErrorKind::StorageFull, None),
            // The bandwidth cap is a wait, not the shared default meaning.
            (
                509,
                Some("60"),
                ErrorKind::RateLimited,
                Some(NOW_MS + 60_000),
            ),
            (423, None, ErrorKind::Transient, None),
            (403, None, ErrorKind::Unauthorized, None),
            (412, None, ErrorKind::PreconditionFailed, None),
            (503, Some("5"), ErrorKind::Transient, Some(NOW_MS + 5_000)),
        ];
        for (status, retry_after, kind, retry_at) in cases {
            let headers = retry_after
                .map(|value: &str| vec![("retry-after", value.to_owned())])
                .unwrap_or_default();
            let (_transport, harness) = scripted(vec![
                exchange(200, &root_folder()),
                exchange(200, &descriptor_page()),
                Exchange {
                    status,
                    headers,
                    body: Vec::new(),
                },
            ]);
            let provider = create(harness.dependencies.clone()).unwrap();
            let cancel = Cancellation::default();
            let (repository, _) = open(
                &provider,
                &config_at("https://graph.microsoft.com/v1.0", "personal"),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .unwrap();
            let error = provider
                .list_objects(&repository, Collection::Snapshots, None, 10, &cancel)
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "{status}");
            assert_eq!(error.http_status, Some(status), "{status}");
            assert_eq!(error.retry_at_ms, retry_at, "{status}");
        }
    });
}

#[test]
fn a_rejected_grant_requires_reauthentication_and_a_rotated_token_is_persisted() {
    runtime().block_on(async {
        // An expired access token triggers a refresh before the first call.
        let expired = MemoryVault::with(
            SECRET,
            format!(
                "{{\"refreshToken\":\"old-refresh\",\"accessToken\":\"stale\",\"accessTokenExpiresAtMs\":{}}}",
                NOW_MS - 1
            )
            .as_bytes(),
        );
        let mut replies = vec![json(
            200,
            "{\"access_token\":\"fresh-access\",\"refresh_token\":\"rotated-refresh\",\"expires_in\":3599,\"token_type\":\"Bearer\"}",
        )];
        replies.extend(existing_open());
        let server = WireServer::start(replies);
        let harness = loopback_dependencies(expired, NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let stored = String::from_utf8(harness.vault.contents(SECRET).unwrap()).unwrap();
        assert!(stored.contains("rotated-refresh") && !stored.contains("old-refresh"));
        assert!(stored.contains("fresh-access"));
        assert!(stored.contains(&(NOW_MS + 3_599_000).to_string()));
        let records = server.requests.lock().unwrap();
        assert_eq!(
            line(&records[0]),
            "POST /synthetic/consumers/oauth2/v2.0/token HTTP/1.1"
        );
        let body = String::from_utf8(records[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("client_id=entra-application"));
        assert!(body.contains("scope=Files.ReadWrite%20offline_access"));
        assert!(!body.contains("client_secret"));
        // The refreshed token is used for the following Graph calls.
        assert!(head(&records[1]).contains("authorization: bearer fresh-access"));
        drop(records);

        let rejected = WireServer::start(vec![json(
            400,
            "{\"error\":\"invalid_grant\",\"error_description\":\"AADSTS70000\"}",
        )]);
        let harness = loopback_dependencies(
            MemoryVault::with(
                SECRET,
                b"{\"refreshToken\":\"revoked\"}",
            ),
            NOW_MS,
        );
        let provider = create(harness.dependencies.clone()).unwrap();
        let error = failed(open(
            &provider,
            &config_for(&rejected, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await);
        assert_eq!(error.kind, ErrorKind::ReauthRequired);
        assert_eq!(error.http_status, Some(400));
        assert_eq!(rejected.requests.lock().unwrap().len(), 1);
    });
}

#[test]
fn a_foreign_handle_or_locator_is_corrupt() {
    runtime().block_on(async {
        let server = WireServer::start(existing_open());
        let harness = harness(NOW_MS);
        let provider = create(harness.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let (repository, _) = open(
            &provider,
            &config_for(&server, "personal"),
            OpenMode::Existing,
            &cancel,
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("unused"), 16).unwrap();

        let foreign_locator = RemoteLocator {
            connection_identity: "onedrive|other".to_owned(),
            collection: None,
            object: "packs/pack-1".to_owned(),
        };
        assert_eq!(
            provider
                .read_object(&repository, &foreign_locator, None, &mut sink, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );

        // A handle from another provider carries a context this adapter cannot use.
        let alien = crate::external_storage::fake::repository();
        let alien_locator = locator_for(&alien, "packs/pack-1");
        assert_eq!(
            provider
                .read_object(&alien, &alien_locator, None, &mut sink, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );

        // A path the adapter could never have written is refused locally.
        let escaping = locator_for(&repository, "packs/../descriptors/head");
        assert_eq!(
            provider
                .read_object(&repository, &escaping, None, &mut sink, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    });
}

#[test]
fn the_authorization_policy_requests_the_scopes_of_its_account_type() {
    let graph = "https://graph.microsoft.com/v1.0";
    let mut config = config_at(graph, "appFolder");
    // Without a registered reply URL there is no policy to start.
    assert!(authorization_policy(&config, "windows").is_err());
    config.location.insert(
        "redirectUri".to_owned(),
        "https://login.microsoftonline.com/common/oauth2/nativeclient".to_owned(),
    );
    config
        .oauth_profile
        .as_mut()
        .unwrap()
        .platform_client_ids
        .insert("android".to_owned(), "android-client".to_owned());

    let policy = authorization_policy(&config, "windows").unwrap();
    assert_eq!(
        policy.authorize_url.as_str(),
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize"
    );
    assert_eq!(policy.client_id, "entra-application");
    assert_eq!(
        policy.scopes,
        vec!["Files.ReadWrite.AppFolder", "offline_access"]
    );
    assert_eq!(
        authorization_policy(&config, "android").unwrap().client_id,
        "android-client"
    );

    let mut business = config_at(graph, "business");
    business.location.insert(
        "redirectUri".to_owned(),
        "risunest://oauth/callback".to_owned(),
    );
    let policy = authorization_policy(&business, "windows").unwrap();
    assert_eq!(policy.scopes, vec!["Files.ReadWrite", "offline_access"]);
    assert_eq!(
        policy.authorize_url.as_str(),
        "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize"
    );
    let (_, authorize) = PendingAuthorization::start(policy).unwrap();
    assert!(authorize
        .query_pairs()
        .any(|(name, value)| name == "code_challenge_method" && value == "S256"));
    assert!(!authorize
        .query_pairs()
        .any(|(name, _)| name == "client_secret"));

    // A reply URL that could leak the code is refused.
    let mut unsafe_redirect = config_at(graph, "business");
    unsafe_redirect.location.insert(
        "redirectUri".to_owned(),
        "http://example.invalid/callback".to_owned(),
    );
    assert!(authorization_policy(&unsafe_redirect, "windows").is_err());
}

#[test]
fn redeeming_an_authorization_code_stores_the_first_token_document() {
    runtime().block_on(async {
        let server = WireServer::start(vec![json(
            200,
            "{\"access_token\":\"first-access\",\"refresh_token\":\"first-refresh\",\"expires_in\":3599,\"token_type\":\"Bearer\"}",
        )]);
        let harness = with_transport(
            Arc::new(crate::external_storage::http::NativeHttpTransport::for_loopback_tests()),
            MemoryVault::default(),
            NOW_MS,
        );
        let connector = OneDrive::new(harness.dependencies.clone());
        let cancel = Cancellation::default();
        let mut config = config_for(&server, "business");
        config.location.insert(
            "redirectUri".to_owned(),
            "risunest://oauth/callback".to_owned(),
        );
        let grant = AuthorizationCode {
            code: crate::external_storage::auth::SecretBytes(zeroize::Zeroizing::new(
                b"synthetic-code".to_vec(),
            )),
            verifier: crate::external_storage::auth::SecretBytes(zeroize::Zeroizing::new(
                b"synthetic-verifier".to_vec(),
            )),
            client_id: "windows-client".to_owned(),
            redirect_url: url::Url::parse("risunest://oauth/callback").unwrap(),
        };
        let stored = connector
            .exchange_authorization_code(&config, &grant, &cancel)
            .await
            .unwrap();
        let document = String::from_utf8(harness.vault.contents(&stored.0).unwrap()).unwrap();
        assert!(document.contains("first-refresh") && document.contains("first-access"));

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            line(&records[0]),
            "POST /synthetic/organizations/oauth2/v2.0/token HTTP/1.1"
        );
        let body = String::from_utf8(records[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("client_id=windows-client"));
        assert!(body.contains("code=synthetic-code"));
        assert!(body.contains("code_verifier=synthetic-verifier"));
        assert!(body.contains("redirect_uri=risunest%3A%2F%2Foauth%2Fcallback"));
        assert!(!body.contains("client_secret"));
    });
}

#[test]
fn request_costs_separate_reads_writes_and_the_identity_platform() {
    let harness = harness(NOW_MS);
    let provider = OneDrive::new(harness.dependencies.clone());
    let names = |operation| {
        cost_model(operation, "onedrive:test")
            .into_iter()
            .map(|cost| {
                assert_eq!(cost.units, 1);
                assert_eq!(cost.reset, QuotaReset::Unknown);
                assert_eq!(cost.shared_account, "onedrive:test");
                cost.bucket
            })
            .collect::<Vec<_>>()
    };
    assert!(provider
        .request_cost(
            &crate::external_storage::fake::repository(),
            ProviderOperation::List
        )
        .is_err());
    assert_eq!(names(ProviderOperation::Authenticate), vec!["auth"]);
    for read in [
        ProviderOperation::Metadata,
        ProviderOperation::List,
        ProviderOperation::DownloadUrl,
        ProviderOperation::Get,
        ProviderOperation::Range,
        ProviderOperation::ReconcileUpload,
    ] {
        assert_eq!(names(read), vec!["requests"]);
    }
    for write in [
        ProviderOperation::Create,
        ProviderOperation::UploadSession,
        ProviderOperation::UploadChunk,
        ProviderOperation::CompleteUpload,
        ProviderOperation::CompareExchangeHead,
        ProviderOperation::ReplaceHead,
    ] {
        assert_eq!(names(write), vec!["requests", "writes"]);
    }
    // A request carries the connection account, not the provider fallback.
    let costs = provider.costs(ProviderOperation::List, "onedrive:9:consumers|9:account-1|");
    assert_eq!(costs[0].shared_account, "onedrive:9:consumers|9:account-1|");
}
