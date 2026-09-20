//! Synthetic wire tests. Every response is scripted by the shared loopback
//! fixture; no account, no token and no network of any kind is involved.
use super::{
    auth::{
        android_web_authorization_policy, authorization_policy, exchange_authorization_code,
        ios_authorization_policy, verify_google_grant,
    },
    config::AuthorizationSettings,
    create,
};
use crate::external_storage::{
    auth::{AuthorizationCode, SecretBytes},
    cleanup::{self, CleanupLimits, CleanupRequest, JobRoots, ObservedRoots, RepositoryView},
    contract::*,
    fake::{
        loopback_dependencies, with_transport, FakeLeaseClock, MemoryVault, TestDependencies,
    },
    http::{HttpRequest, HttpResponse, HttpTransport, NativeHttpTransport},
    leases::LeaseContext,
    packaging::RemoteObject,
    providers::Dependencies,
    quota::AccountKey,
    reachability::{DocumentNode, DocumentSource},
    transfer::{SpoolSink, SpoolSource},
    wire_fixture::{Reply, WireServer},
};
use risunest_external_storage_format::format::{Descriptor, Strategy};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

const NOW_MS: u64 = 1_700_000_000_000;
const ACCOUNT: &str = "permission-1";
const FOLDER: &str = "folder-root";
const SECRET: &str = "google-drive-secret";
const PROJECT_NUMBER: &str = "123456789012";

fn client_id(platform: &str) -> String {
    format!("{PROJECT_NUMBER}-{platform}.apps.googleusercontent.com")
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn platform_client_ids() -> BTreeMap<String, String> {
    ["windows", "android", "macos", "ios", "linux"]
        .iter()
        .map(|platform| ((*platform).to_owned(), client_id(platform)))
        .collect()
}

fn connection(endpoint: &str) -> ConnectionConfig {
    let mut location = BTreeMap::new();
    location.insert("folderId".to_owned(), FOLDER.to_owned());
    ConnectionConfig {
        provider: "google_drive".to_owned(),
        profile: None,
        endpoint: endpoint.to_owned(),
        account_id: ACCOUNT.to_owned(),
        location,
        oauth_profile: Some(OAuthProfile {
            project_id: "project-1".to_owned(),
            platform_client_ids: platform_client_ids(),
        }),
    }
}

fn stored_secret(expires_at_ms: u64) -> Vec<u8> {
    format!(
        "{{\"refreshToken\":\"synthetic-refresh\",\"accessToken\":\"synthetic-access\",\"accessTokenExpiresAtMs\":{expires_at_ms}}}"
    )
    .into_bytes()
}

fn stored_secret_with_client_secret(expires_at_ms: u64) -> Vec<u8> {
    format!(
        "{{\"refreshToken\":\"synthetic-refresh\",\"accessToken\":\"synthetic-access\",\"accessTokenExpiresAtMs\":{expires_at_ms},\"clientSecret\":\"synthetic-client-secret\"}}"
    )
    .into_bytes()
}

fn deps_with(secret: Option<Vec<u8>>) -> TestDependencies {
    let vault = match secret {
        Some(bytes) => MemoryVault::with(SECRET, &bytes),
        None => MemoryVault::default(),
    };
    loopback_dependencies(vault, NOW_MS)
}

struct RequestIdentity {
    authorization: Option<String>,
    account: AccountKey,
    api_request: bool,
}

struct InspectingTransport {
    inner: NativeHttpTransport,
    requests: Mutex<Vec<RequestIdentity>>,
}

impl InspectingTransport {
    fn new() -> Self {
        Self {
            inner: NativeHttpTransport::for_loopback_tests(),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl HttpTransport for InspectingTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HttpResponse> {
        self.requests.lock().unwrap().push(RequestIdentity {
            authorization: request.headers.get("authorization").cloned(),
            account: request.account.clone(),
            api_request: request.api_request,
        });
        self.inner.send(request, cancel)
    }
}

fn deps_with_inspection(
    secret: Vec<u8>,
) -> (Arc<InspectingTransport>, TestDependencies) {
    let transport = Arc::new(InspectingTransport::new());
    let dependencies = with_transport(
        transport.clone(),
        MemoryVault::with(SECRET, &secret),
        NOW_MS,
    );
    (transport, dependencies)
}

fn provider_of(dependencies: &Dependencies) -> std::sync::Arc<dyn Provider> {
    create(dependencies.clone()).unwrap()
}

fn secret_ref() -> SecretRef {
    SecretRef(SECRET.to_owned())
}

fn json_reply(status: u16, body: Value) -> Reply {
    Reply::Http {
        status,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body: body.to_string().into_bytes(),
    }
}
fn json_reply_with(status: u16, headers: &[(&str, &str)], body: Value) -> Reply {
    Reply::Http {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: body.to_string().into_bytes(),
    }
}
fn error_reply(status: u16, reason: &str) -> Reply {
    json_reply(
        status,
        json!({ "error": { "errors": [{ "reason": reason }] } }),
    )
}

fn about_reply() -> Reply {
    json_reply(200, json!({ "user": { "permissionId": ACCOUNT } }))
}
fn folder_reply() -> Reply {
    json_reply(
        200,
        json!({
            "id": FOLDER,
            "mimeType": "application/vnd.google-apps.folder",
            "trashed": false
        }),
    )
}
fn control_reply(files: Vec<Value>) -> Reply {
    json_reply(200, json!({ "files": files }))
}
fn ids_reply(id: &str) -> Reply {
    json_reply(200, json!({ "ids": [id] }))
}
fn head_file(id: &str, version: &str) -> Value {
    json!({
        "id": id,
        "name": "control-head",
        "size": "12",
        "version": version,
        "appProperties": { "risunestRole": "head" }
    })
}
fn descriptor_file(id: &str) -> Value {
    json!({
        "id": id,
        "name": "descriptor-d1",
        "size": "20",
        "version": "3",
        "appProperties": {
            "risunestRole": "descriptor",
            "risunestObjectId": "d1",
            "risunestJobId": "d1"
        }
    })
}

/// Replies for an open of an existing repository that already has a head.
fn open_existing_replies() -> Vec<Reply> {
    vec![
        about_reply(),
        folder_reply(),
        control_reply(vec![head_file("head-file", "7"), descriptor_file("desc-1")]),
    ]
}

fn hash(bytes: &[u8]) -> String {
    risunest_sync_wire::hash(bytes)
}

fn spool(directory: &std::path::Path, name: &str, bytes: &[u8]) -> SpoolSource {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    SpoolSource::verified(&path, bytes.len() as u64, &hash(bytes)).unwrap()
}

fn intent(
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
        sha256: hash(bytes),
    }
}

fn request_lines(server: &WireServer) -> Vec<String> {
    server
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| {
            request
                .headers
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

struct UnusedCleanupView;
impl RepositoryView for UnusedCleanupView {
    fn roots<'a>(&'a self, _: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots> {
        Box::pin(async { Ok(ObservedRoots::default()) })
    }

    fn snapshots<'a>(&'a self, _: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn job_roots(&self) -> Result<JobRoots> {
        Ok(JobRoots::default())
    }

    fn known_objects(&self) -> Result<Vec<RemoteObject>> {
        Ok(Vec::new())
    }
}

struct UnusedCleanupDocuments;
impl DocumentSource for UnusedCleanupDocuments {
    fn document<'a>(&'a self, _: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode> {
        Box::pin(async { Err(ProviderError::new(ErrorKind::Corrupt)) })
    }

    fn listed<'a>(
        &'a self,
        _: &'a ObjectReceipt,
    ) -> ProviderFuture<'a, (RemoteObject, DocumentNode)> {
        Box::pin(async { Err(ProviderError::new(ErrorKind::Corrupt)) })
    }

    fn catalog<'a>(&'a self, _: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>> {
        Box::pin(async { Err(ProviderError::new(ErrorKind::Corrupt)) })
    }

    fn probe<'a>(&'a self, _: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
        Box::pin(async { Err(ProviderError::new(ErrorKind::Corrupt)) })
    }
}

#[test]
fn configuration_validation_refuses_foreign_and_incomplete_connections() {
    runtime().block_on(async {
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        let endpoint = "http://127.0.0.1:1/synthetic";
        let mut cases: Vec<ConnectionConfig> = Vec::new();
        let mut foreign = connection(endpoint);
        foreign.provider = "onedrive".to_owned();
        cases.push(foreign);
        cases.push(connection("http://example.invalid/drive"));
        let mut no_folder = connection(endpoint);
        no_folder.location.clear();
        cases.push(no_folder);
        let mut unknown_key = connection(endpoint);
        unknown_key
            .location
            .insert("bucket".to_owned(), "x".to_owned());
        cases.push(unknown_key);
        let mut bad_space = connection(endpoint);
        bad_space
            .location
            .insert("space".to_owned(), "photos".to_owned());
        cases.push(bad_space);
        let mut app_data_folder = connection(endpoint);
        app_data_folder
            .location
            .insert("space".to_owned(), "appDataFolder".to_owned());
        cases.push(app_data_folder);
        let mut no_account = connection(endpoint);
        no_account.account_id.clear();
        cases.push(no_account);
        let mut no_oauth = connection(endpoint);
        no_oauth.oauth_profile = None;
        cases.push(no_oauth);
        let mut no_client = connection(endpoint);
        no_client
            .oauth_profile
            .as_mut()
            .unwrap()
            .platform_client_ids
            .clear();
        cases.push(no_client);
        let mut wrong_profile = connection(endpoint);
        wrong_profile.profile = Some("appdata".to_owned());
        cases.push(wrong_profile);
        let mut mixed_projects = connection(endpoint);
        mixed_projects
            .oauth_profile
            .as_mut()
            .unwrap()
            .platform_client_ids
            .insert(
                "android".to_owned(),
                "999999999999-android.apps.googleusercontent.com".to_owned(),
            );
        cases.push(mixed_projects);
        for config in &cases {
            assert_eq!(
                provider
                    .open_repository(config, &secret_ref(), OpenMode::Existing, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        // A valid configuration whose secret is gone needs a new authorization.
        let missing = deps_with(None);
        let provider = provider_of(&missing.dependencies);
        assert_eq!(
            provider
                .open_repository(
                    &connection(endpoint),
                    &secret_ref(),
                    OpenMode::Existing,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::ReauthRequired
        );
    });
}

#[test]
fn open_existing_verifies_identity_folder_and_control_objects() {
    runtime().block_on(async {
        let server = WireServer::start(open_existing_replies());
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        let (repository, capabilities) = provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(
            repository.connection_identity,
            format!(
                "google_drive|{}|drive|{ACCOUNT}|{FOLDER}",
                server.url.as_str()
            )
        );
        assert_eq!(
            repository.repository_id,
            format!("google_drive:{ACCOUNT}:{FOLDER}")
        );
        assert!(capabilities.immutable_create);
        assert!(capabilities.stable_head_replace);
        assert!(capabilities.head_read_after_write);
        assert!(capabilities.head_retry_control);
        assert!(capabilities.snapshot_discovery);
        assert!(!capabilities.atomic_create_head);
        assert!(!capabilities.conditional_head_update);
        assert!(capabilities
            .require(PublicationStrategy::Sequential)
            .is_ok());
        assert_eq!(
            capabilities
                .require(PublicationStrategy::Cas)
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unsupported
        );
        assert!(!capabilities.conditional_get);
        assert!(capabilities.range && capabilities.resumable_upload);
        assert_eq!(capabilities.upload_alignment, 256 * 1024);

        let lines = request_lines(&server);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("GET /synthetic/drive/v3/about?fields=user"));
        assert!(lines[1].starts_with(&format!("GET /synthetic/drive/v3/files/{FOLDER}?fields=")));
        assert!(lines[2].contains("/drive/v3/files?q="));
        assert!(lines[2].contains("risunestRole"));
        assert!(lines[2].contains("pageSize=100"));
    });
}

#[test]
fn create_mode_refuses_a_location_that_already_holds_control_objects() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![descriptor_file("desc-1")]),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        assert_eq!(
            provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Create,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );
        // Nothing was written: only the three discovery reads happened.
        assert_eq!(request_lines(&server).len(), 3);

        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![json!({
                "id": "pack-1",
                "name": "pack-object-1",
                "mimeType": "application/octet-stream",
                "parents": [FOLDER],
                "size": "4",
                "appProperties": {
                    "risunestRole": "pack",
                    "risunestObjectId": "object-1"
                }
            })]),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        assert_eq!(
            provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Create,
                    &cancel,
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(request_lines(&server).len(), 3);
    });
}

#[test]
fn create_mode_reserves_a_head_identifier_in_an_empty_location() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![]),
            ids_reply("reserved-head"),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        let (repository, _) = provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Create,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(
            provider.head_locator(&repository).unwrap().object,
            "control-head"
        );
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 4);
        assert!(lines[3].contains("/drive/v3/files/generateIds?count=1&space=drive"));
    });
}

#[test]
fn resume_create_accepts_only_an_empty_or_bootstrap_descriptor_control_layout() {
    runtime().block_on(async {
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();

        let empty = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![]),
            ids_reply("reserved-head"),
        ]);
        provider
            .open_repository(
                &connection(empty.url.as_str()),
                &secret_ref(),
                OpenMode::ResumeCreate,
                &cancel,
            )
            .await
            .unwrap();
        let empty_lines = request_lines(&empty);
        assert_eq!(empty_lines.len(), 4);
        assert!(!empty_lines[2].contains("risunestRole"));

        let published = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![descriptor_file("desc-1")]),
            ids_reply("reserved-head"),
            control_reply(vec![descriptor_file("desc-1")]),
        ]);
        let (published_repository, _) = provider
            .open_repository(
                &connection(published.url.as_str()),
                &secret_ref(),
                OpenMode::ResumeCreate,
                &cancel,
            )
            .await
            .unwrap();
        let descriptor_intent = ObjectIntent {
            repository_id: published_repository.repository_id.clone(),
            job_id: "d1".to_owned(),
            object_id: "d1".to_owned(),
            role: ObjectRole::Descriptor,
            byte_length: 20,
            sha256: "00".repeat(32),
        };
        assert!(provider
            .begin_upload(&published_repository, &descriptor_intent, &cancel)
            .await
            .unwrap()
            .is_none());
        assert!(request_lines(&published)
            .iter()
            .all(|line| line.starts_with("GET ")));

        let with_head = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![head_file("head-file", "7"), descriptor_file("desc-1")]),
        ]);
        assert_eq!(
            provider
                .open_repository(
                    &connection(with_head.url.as_str()),
                    &secret_ref(),
                    OpenMode::ResumeCreate,
                    &cancel,
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );

        let too_many = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![
                descriptor_file("desc-1"),
                descriptor_file("desc-2"),
                descriptor_file("desc-3"),
            ]),
        ]);
        assert_eq!(
            provider
                .open_repository(
                    &connection(too_many.url.as_str()),
                    &secret_ref(),
                    OpenMode::ResumeCreate,
                    &cancel,
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );

        for (label, hidden) in [
            (
                "payload",
                json!({
                    "id": "pack-1",
                    "name": "pack-p1",
                    "size": "20",
                    "version": "3",
                    "appProperties": {
                        "risunestRole": "pack",
                        "risunestObjectId": "p1",
                        "risunestJobId": "p1"
                    }
                }),
            ),
            (
                "foreign file",
                json!({ "id": "foreign-1", "name": "notes.txt", "size": "20", "version": "3" }),
            ),
        ] {
            let server = WireServer::start(vec![
                about_reply(),
                folder_reply(),
                control_reply(vec![hidden]),
            ]);
            assert_eq!(
                provider
                    .open_repository(
                        &connection(server.url.as_str()),
                        &secret_ref(),
                        OpenMode::ResumeCreate,
                        &cancel,
                    )
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::PreconditionFailed,
                "{label}"
            );
            assert_eq!(request_lines(&server).len(), 3, "{label}");
        }

        let mut malformed = descriptor_file("desc-1");
        malformed["size"] = json!("0");
        let malformed = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![malformed]),
        ]);
        assert_eq!(
            provider
                .open_repository(
                    &connection(malformed.url.as_str()),
                    &secret_ref(),
                    OpenMode::ResumeCreate,
                    &cancel,
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
    });
}

#[test]
fn a_new_device_without_access_to_the_files_is_not_found_and_never_creates() {
    runtime().block_on(async {
        // An unrelated OAuth application cannot see files granted per file.
        let server = WireServer::start(vec![about_reply(), error_reply(404, "notFound")]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        assert_eq!(
            provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Existing,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );
        assert_eq!(request_lines(&server).len(), 2);

        // The same project on another device sees the folder but no descriptor.
        let server = WireServer::start(vec![about_reply(), folder_reply(), control_reply(vec![])]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        assert_eq!(
            provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Existing,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );
        assert_eq!(request_lines(&server).len(), 3);

        // Another account's credentials never operate on this connection.
        let server = WireServer::start(vec![json_reply(
            200,
            json!({ "user": { "permissionId": "permission-other" } }),
        )]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        assert_eq!(
            provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Existing,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unauthorized
        );
    });
}

#[test]
fn several_files_with_the_same_control_name_are_corrupt() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![
                head_file("head-a", "7"),
                head_file("head-b", "9"),
                descriptor_file("desc-1"),
            ]),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        assert_eq!(
            provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Existing,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
    });
}

#[test]
fn an_expired_access_token_is_refreshed_once_and_rotation_is_persisted() {
    runtime().block_on(async {
        let mut replies = vec![json_reply(
            200,
            json!({
                "access_token": "refreshed-access",
                "expires_in": 3599,
                "refresh_token": "rotated-refresh"
            }),
        )];
        replies.extend(open_existing_replies());
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret_with_client_secret(NOW_MS - 1)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .unwrap();
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("POST /synthetic/token"));
        let records = server.requests.lock().unwrap();
        let body = String::from_utf8(records[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=synthetic-refresh"));
        assert!(body.contains("client_id="));
        assert!(body.contains("client_secret=synthetic-client-secret"));
        assert!(records[1]
            .headers
            .to_lowercase()
            .contains("authorization: bearer refreshed-access"));
        let stored = String::from_utf8(test.vault.contents(SECRET).unwrap()).unwrap();
        assert!(stored.contains("rotated-refresh"));
        assert!(stored.contains("refreshed-access"));
        assert!(stored.contains("synthetic-client-secret"));
        assert!(!stored.contains("synthetic-refresh"));
    });
}

#[test]
fn a_rejected_token_is_refreshed_once_and_an_invalid_grant_requires_reauthorization() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            error_reply(401, "authError"),
            json_reply(400, json!({ "error": "invalid_grant" })),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        let failure = provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(failure.kind, ErrorKind::ReauthRequired);
        assert_eq!(failure.http_status, Some(400));
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("POST /synthetic/token"));
    });
}

#[test]
fn a_rejected_token_is_retried_once_after_a_successful_refresh() {
    runtime().block_on(async {
        let mut replies = vec![
            error_reply(401, "authError"),
            json_reply(
                200,
                json!({ "access_token": "second-access", "expires_in": 3599 }),
            ),
        ];
        replies.extend(open_existing_replies());
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .unwrap();
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("GET /synthetic/drive/v3/about"));
        assert!(lines[1].starts_with("POST /synthetic/token"));
        assert!(lines[2].starts_with("GET /synthetic/drive/v3/about"));
    });
}

#[test]
fn documented_throttling_storage_and_quota_reasons_are_distinguished() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let cases: Vec<(Reply, ErrorKind, Option<u64>)> = vec![
            (
                error_reply(403, "userRateLimitExceeded"),
                ErrorKind::RateLimited,
                None,
            ),
            (
                error_reply(403, "rateLimitExceeded"),
                ErrorKind::RateLimited,
                None,
            ),
            (
                error_reply(403, "dailyLimitExceeded"),
                ErrorKind::DailyQuotaExhausted,
                None,
            ),
            (
                error_reply(403, "storageQuotaExceeded"),
                ErrorKind::StorageFull,
                None,
            ),
            (
                error_reply(403, "insufficientFilePermissions"),
                ErrorKind::Unauthorized,
                None,
            ),
            (
                json_reply_with(429, &[("Retry-After", "30")], json!({})),
                ErrorKind::RateLimited,
                Some(NOW_MS + 30_000),
            ),
            (json_reply(503, json!({})), ErrorKind::Transient, None),
        ];
        for (reply, kind, retry_at_ms) in cases {
            let server = WireServer::start(vec![reply]);
            let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
            let provider = provider_of(&test.dependencies);
            let failure = provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Existing,
                    &cancel,
                )
                .await
                .err()
                .unwrap();
            assert_eq!(failure.kind, kind, "{:?}", failure.http_status);
            if retry_at_ms.is_some() {
                assert_eq!(failure.retry_at_ms, retry_at_ms);
            }
        }
    });
}

async fn opened(
    server: &WireServer,
    test: &TestDependencies,
    cancel: &Cancellation,
) -> (std::sync::Arc<dyn Provider>, RepositoryHandle) {
    let provider = provider_of(&test.dependencies);
    let (repository, _) = provider
        .open_repository(
            &connection(server.url.as_str()),
            &secret_ref(),
            OpenMode::Existing,
            cancel,
        )
        .await
        .unwrap();
    (provider, repository)
}

#[test]
fn reading_streams_verified_bytes_and_reports_an_unchanged_version() {
    runtime().block_on(async {
        let payload = vec![9u8; 4096];
        let digest = hash(&payload);
        let metadata = json!({
            "id": "pack-file",
            "size": payload.len().to_string(),
            "version": "11",
            "sha256Checksum": digest,
        });
        let mut replies = open_existing_replies();
        replies.push(json_reply(200, metadata.clone()));
        replies.push(json_reply(200, metadata.clone()));
        replies.push(Reply::Http {
            status: 200,
            headers: vec![],
            body: payload.clone(),
        });
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: "pack-file".to_owned(),
        };
        let directory = tempfile::tempdir().unwrap();
        let mut unchanged_sink =
            SpoolSink::create(&directory.path().join("unchanged"), payload.len() as u64).unwrap();
        let receipt = provider
            .read_object(
                &repository,
                &locator,
                Some(&VersionToken("11".to_owned())),
                &mut unchanged_sink,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(receipt, ReadReceipt::NotModified(VersionToken("11".into())));
        assert_eq!(request_lines(&server).len(), 4);

        let mut sink =
            SpoolSink::create(&directory.path().join("body"), payload.len() as u64).unwrap();
        let receipt = provider
            .read_object(&repository, &locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        let ReadReceipt::Body(receipt) = receipt else {
            panic!("expected a streamed body");
        };
        assert_eq!(receipt.byte_length, payload.len() as u64);
        assert_eq!(receipt.version, Some(VersionToken("11".to_owned())));
        assert_eq!(
            receipt.checksum,
            Some(Checksum {
                algorithm: "sha256".to_owned(),
                value: digest.clone(),
                provider_verified: true,
            })
        );
        assert!(receipt.complete);
        assert!(sink.is_verified());
        assert_eq!(
            std::fs::read(directory.path().join("body")).unwrap(),
            payload
        );
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 6);
        assert!(lines[5].contains("/drive/v3/files/pack-file?alt=media"));
    });
}

#[test]
fn a_foreign_handle_or_locator_is_refused() {
    runtime().block_on(async {
        let server = WireServer::start(open_existing_replies());
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("sink"), 16).unwrap();
        let foreign = RemoteLocator {
            connection_identity: "google_drive|other|drive|other|other".to_owned(),
            collection: None,
            object: "pack-file".to_owned(),
        };
        assert_eq!(
            provider
                .read_object(&repository, &foreign, None, &mut sink, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        let unusable = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: "not/a/file/id".to_owned(),
        };
        assert_eq!(
            provider
                .read_object(&repository, &unusable, None, &mut sink, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        let other = crate::external_storage::fake::repository();
        assert_eq!(
            provider
                .read_object(
                    &other,
                    &crate::external_storage::fake::locator(),
                    None,
                    &mut sink,
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(request_lines(&server).len(), 3);
    });
}

#[test]
fn an_immutable_create_converges_after_a_conflict_and_refuses_different_bytes() {
    runtime().block_on(async {
        let payload = vec![3u8; 2048];
        let digest = hash(&payload);
        let stored = json!({
            "id": "pack-generated",
            "size": payload.len().to_string(),
            "version": "4",
            "sha256Checksum": digest,
            "appProperties": { "risunestRole": "pack", "risunestObjectId": "object-1" }
        });
        let mut replies = open_existing_replies();
        // First attempt: nothing stored yet, the create response is lost.
        replies.push(json_reply(200, json!({ "files": [] })));
        replies.push(ids_reply("pack-generated"));
        replies.push(Reply::Lost);
        // Second attempt: the identifier is taken by the earlier attempt.
        replies.push(json_reply(200, json!({ "files": [] })));
        replies.push(ids_reply("pack-generated"));
        replies.push(error_reply(409, "alreadyExists"));
        // Third attempt: the stored object is found and converges.
        replies.push(json_reply(200, json!({ "files": [stored.clone()] })));
        // Fourth: a different length under the same identity never overwrites.
        replies.push(json_reply(
            200,
            json!({ "files": [json!({
                "id": "pack-generated",
                "size": "99",
                "version": "4",
                "appProperties": { "risunestRole": "pack", "risunestObjectId": "object-1" }
            })] }),
        ));
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "pack", &payload);
        let intent = intent(&repository, "object-1", ObjectRole::Pack, &payload);

        assert_eq!(
            provider
                .create_object(&repository, &intent, &source, None, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Transient
        );
        assert_eq!(
            provider
                .create_object(&repository, &intent, &source, None, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );
        let receipt = provider
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "pack-generated");
        assert_eq!(receipt.locator.collection, None);
        assert_eq!(receipt.byte_length, payload.len() as u64);
        assert!(receipt.complete);
        assert!(receipt.checksum.unwrap().provider_verified);
        assert_eq!(
            provider
                .create_object(&repository, &intent, &source, None, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 11);
        assert!(lines[3].contains("risunestObjectId"));
        assert!(lines[4].contains("/drive/v3/files/generateIds"));
        assert!(lines[5].contains("/upload/drive/v3/files?uploadType=multipart"));
        let records = server.requests.lock().unwrap();
        let body = String::from_utf8_lossy(&records[5].body).to_string();
        assert!(body.contains("\"risunestObjectId\":\"object-1\""));
        assert!(body.contains("\"risunestJobId\":\"job-1\""));
        assert!(body.contains("\"name\":\"pack-object-1\""));
        assert!(records[5].body.windows(8).any(|window| window == [3u8; 8]));
    });
}

#[test]
fn a_resumable_session_continues_from_the_offset_the_service_confirmed() {
    runtime().block_on(async {
        let payload = vec![5u8; 1024 * 1024];
        let digest = hash(&payload);
        let mut replies = open_existing_replies();
        replies.push(ids_reply("pack-session"));
        replies.push(json_reply_with(
            200,
            &[("Location", "/synthetic/upload/session/one")],
            json!({}),
        ));
        replies.push(json_reply_with(
            308,
            &[("Range", "bytes=0-524287")],
            json!({}),
        ));
        replies.push(json_reply(
            200,
            json!({
                "id": "pack-session",
                "size": payload.len().to_string(),
                "version": "6",
                "sha256Checksum": digest,
            }),
        ));
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "pack", &payload);
        let intent = intent(&repository, "object-2", ObjectRole::SyncState, &payload);
        let resume = provider
            .begin_upload(&repository, &intent, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resume.confirmed_offset, 0);
        assert_eq!(resume.expires_at_ms, Some(NOW_MS + 7 * 24 * 60 * 60 * 1000));
        let sealed = String::from_utf8(
            test.vault
                .contents(&resume.sealed_state.0)
                .expect("sealed upload state"),
        )
        .unwrap();
        assert!(sealed.contains("/upload/session/one"));
        assert!(sealed.contains("pack-session"));

        let receipt = provider
            .create_object(&repository, &intent, &source, Some(&resume), &cancel)
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "pack-session");
        assert_eq!(receipt.locator.collection.as_deref(), Some("snapshots"));
        assert!(receipt.complete);
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 7);
        assert!(records[4]
            .headers
            .to_lowercase()
            .contains("x-upload-content-length: 1048576"));
        assert!(records[5]
            .headers
            .to_lowercase()
            .contains("content-range: bytes 0-1048575/1048576"));
        assert_eq!(records[5].body.len(), 1024 * 1024);
        assert!(records[6]
            .headers
            .to_lowercase()
            .contains("content-range: bytes 524288-1048575/1048576"));
        assert_eq!(records[6].body.len(), 524_288);
        assert!(records[5]
            .headers
            .to_lowercase()
            .contains("/synthetic/upload/session/one"));
        drop(records);
    });
}

#[test]
fn an_expired_session_restarts_and_a_confirmed_one_completes() {
    runtime().block_on(async {
        let payload = vec![7u8; 4096];
        let digest = hash(&payload);
        let mut replies = open_existing_replies();
        replies.push(ids_reply("pack-expired"));
        replies.push(json_reply_with(200, &[("Location", "self")], json!({})));
        // Reconcile: the session is gone and the file was never stored.
        replies.push(json_reply(404, json!({})));
        replies.push(error_reply(404, "notFound"));
        // A second reconcile finds a confirmed offset.
        replies.push(json_reply_with(
            308,
            &[("Range", "bytes=0-2047")],
            json!({}),
        ));
        // A third reconcile finds the completed object.
        replies.push(json_reply(
            200,
            json!({
                "id": "pack-expired",
                "size": payload.len().to_string(),
                "version": "8",
                "sha256Checksum": digest,
            }),
        ));
        // A fourth reconcile finds a different object under the same identity.
        replies.push(json_reply(
            200,
            json!({ "id": "pack-expired", "size": "10", "version": "9" }),
        ));
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let intent = intent(&repository, "object-3", ObjectRole::Pack, &payload);
        let resume = provider
            .begin_upload(&repository, &intent, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            provider
                .reconcile_upload(&repository, &intent, Some(&resume), &cancel)
                .await
                .unwrap(),
            UploadResolution::RestartRequired
        ));
        let UploadResolution::Resumable(updated) = provider
            .reconcile_upload(&repository, &intent, Some(&resume), &cancel)
            .await
            .unwrap()
        else {
            panic!("expected a resumable session");
        };
        assert_eq!(updated.confirmed_offset, 2048);
        let UploadResolution::Complete(receipt) = provider
            .reconcile_upload(&repository, &intent, Some(&resume), &cancel)
            .await
            .unwrap()
        else {
            panic!("expected a completed upload");
        };
        assert_eq!(receipt.byte_length, payload.len() as u64);
        assert!(receipt.checksum.unwrap().provider_verified);
        assert!(matches!(
            provider
                .reconcile_upload(&repository, &intent, Some(&resume), &cancel)
                .await
                .unwrap(),
            UploadResolution::Conflict
        ));
    });
}

#[test]
fn a_mismatched_identifier_or_digest_is_never_reported_as_complete() {
    runtime().block_on(async {
        let payload = vec![1u8; 512];
        let mut replies = open_existing_replies();
        replies.push(json_reply(
            200,
            json!({
                "id": "pack-file",
                "size": payload.len().to_string(),
                "version": "3",
                "sha256Checksum": hash(b"different-bytes"),
            }),
        ));
        replies.push(Reply::Http {
            status: 200,
            headers: vec![],
            body: payload.clone(),
        });
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("body"), 512).unwrap();
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: "pack-file".to_owned(),
        };
        assert_eq!(
            provider
                .read_object(&repository, &locator, None, &mut sink, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );

        // A completion naming another file is never the object of this job.
        let mut replies = open_existing_replies();
        replies.push(ids_reply("pack-wanted"));
        replies.push(json_reply_with(
            200,
            &[("Location", "/synthetic/upload/session/two")],
            json!({}),
        ));
        replies.push(json_reply(
            200,
            json!({
                "id": "pack-other",
                "size": payload.len().to_string(),
                "version": "4",
                "sha256Checksum": hash(&payload),
            }),
        ));
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let source = spool(directory.path(), "pack-mismatch", &payload);
        let intent = intent(&repository, "object-9", ObjectRole::Pack, &payload);
        let resume = provider
            .begin_upload(&repository, &intent, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            provider
                .create_object(&repository, &intent, &source, Some(&resume), &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
    });
}

#[test]
fn a_head_replacement_writes_once_and_is_readable_afterwards() {
    runtime().block_on(async {
        let head_bytes = b"synthetic-head-bytes".to_vec();
        let digest = hash(&head_bytes);
        let mut replies = open_existing_replies();
        replies.push(json_reply(
            200,
            json!({
                "id": "head-file",
                "size": head_bytes.len().to_string(),
                "version": "8",
                "sha256Checksum": digest,
            }),
        ));
        replies.push(json_reply(
            200,
            json!({
                "id": "head-file",
                "size": head_bytes.len().to_string(),
                "version": "8",
                "sha256Checksum": digest,
            }),
        ));
        replies.push(Reply::Http {
            status: 200,
            headers: vec![],
            body: head_bytes.clone(),
        });
        replies.push(Reply::Lost);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let locator = provider.head_locator(&repository).unwrap();
        let head = HeadBytes::new(head_bytes.clone()).unwrap();
        let receipt = provider
            .replace_head(&repository, &locator, &head, &cancel)
            .await
            .unwrap();
        assert_eq!(receipt.version, Some(VersionToken("8".to_owned())));
        assert!(receipt.complete);
        assert_eq!(request_lines(&server).len(), 4);

        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("head"), 4096).unwrap();
        let read = provider
            .read_object(&repository, &locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        let ReadReceipt::Body(read) = read else {
            panic!("expected the head body");
        };
        assert_eq!(read.byte_length, head_bytes.len() as u64);
        assert_eq!(
            std::fs::read(directory.path().join("head")).unwrap(),
            head_bytes
        );

        // A lost response is never covered by a second write attempt.
        let before = request_lines(&server).len();
        assert_eq!(
            provider
                .replace_head(&repository, &locator, &head, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Transient
        );
        assert_eq!(request_lines(&server).len(), before + 1);
        let records = server.requests.lock().unwrap();
        assert!(records[3]
            .headers
            .starts_with("PATCH /synthetic/upload/drive/v3/files/head-file?uploadType=media"));
        assert_eq!(records[3].body, head_bytes);
        drop(records);
    });
}

#[test]
fn a_missing_head_is_created_once_with_the_reserved_identifier() {
    runtime().block_on(async {
        let head_bytes = b"first-head".to_vec();
        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            control_reply(vec![descriptor_file("desc-1")]),
            ids_reply("reserved-head"),
            json_reply(
                200,
                json!({ "id": "reserved-head", "size": head_bytes.len().to_string(), "version": "1" }),
            ),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let locator = provider.head_locator(&repository).unwrap();
        let receipt = provider
            .replace_head(
                &repository,
                &locator,
                &HeadBytes::new(head_bytes.clone()).unwrap(),
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(receipt.version, Some(VersionToken("1".to_owned())));
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 5);
        assert!(records[4]
            .headers
            .starts_with("POST /synthetic/upload/drive/v3/files?uploadType=multipart"));
        let body = String::from_utf8_lossy(&records[4].body).to_string();
        assert!(body.contains("\"id\":\"reserved-head\""));
        assert!(body.contains("\"name\":\"control-head\""));
        assert!(body.contains("first-head"));
    });
}

#[test]
fn compare_and_exchange_is_unsupported_rather_than_emulated() {
    runtime().block_on(async {
        let server = WireServer::start(open_existing_replies());
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let locator = provider.head_locator(&repository).unwrap();
        let head = HeadBytes::new(b"head".to_vec()).unwrap();
        for expected in [
            ExpectedHead::Absent,
            ExpectedHead::Exact(VersionToken("7".to_owned())),
        ] {
            assert_eq!(
                provider
                    .compare_exchange_head(&repository, &locator, &expected, &head, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        assert_eq!(request_lines(&server).len(), 3);
    });
}

#[test]
fn listing_accepts_false_and_absent_incomplete_search_with_a_cursor() {
    runtime().block_on(async {
        let mut replies = open_existing_replies();
        replies.push(json_reply(
            200,
            json!({
                "files": [
                    { "id": "snap-1", "size": "10", "version": "2",
                      "appProperties": { "risunestRole": "state", "risunestObjectId": "s1" } }
                ],
                "incompleteSearch": false,
                "nextPageToken": "page-2"
            }),
        ));
        replies.push(json_reply(
            200,
            json!({
                "files": [
                    { "id": "snap-2", "size": "12", "version": "3",
                      "sha256Checksum": hash(b"snapshot-two"),
                      "appProperties": { "risunestRole": "state", "risunestObjectId": "s2" } }
                ]
            }),
        ));
        let server = WireServer::start(replies);
        let (transport, test) =
            deps_with_inspection(stored_secret(NOW_MS + 3_600_000));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        for limit in [0u16, 1001] {
            assert_eq!(
                provider
                    .list_objects(&repository, Collection::Snapshots, None, limit, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        let first = provider
            .list_objects(&repository, Collection::Snapshots, None, 1, &cancel)
            .await
            .unwrap();
        assert_eq!(first.objects.len(), 1);
        assert_eq!(first.objects[0].locator.object, "snap-1");
        assert_eq!(
            first.objects[0].locator.collection.as_deref(),
            Some("snapshots")
        );
        assert_eq!(first.next_cursor.as_deref(), Some("page-2"));
        let second = provider
            .list_objects(
                &repository,
                Collection::Snapshots,
                first.next_cursor.as_deref(),
                1,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(second.objects[0].locator.object, "snap-2");
        assert!(
            !second.objects[0]
                .checksum
                .as_ref()
                .unwrap()
                .provider_verified
        );
        assert_eq!(second.next_cursor, None);
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 5);
        assert!(lines[3].contains("pageSize=1"));
        assert!(lines[3].contains("value%3D%27state%27"));
        assert!(lines[4].contains("pageToken=page-2"));
        assert!(server.requests.lock().unwrap()[4]
            .headers
            .to_lowercase()
            .contains("authorization: bearer synthetic-access"));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        let second_page = &requests[4];
        assert_eq!(
            second_page.authorization.as_deref(),
            Some("Bearer synthetic-access")
        );
        assert_eq!(second_page.account.provider(), "google_drive");
        assert_eq!(second_page.account.principal(), ACCOUNT);
        assert_eq!(
            second_page.account.authority(),
            server.url.origin().ascii_serialization()
        );
        assert!(!second_page.account.is_pending());
        assert!(second_page.api_request);
    });
}

#[test]
fn second_control_page_authorization_failure_aborts_open_without_a_later_request() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            json_reply(
                200,
                json!({
                    "files": [descriptor_file("desc-1")],
                    "nextPageToken": "page-2"
                }),
            ),
            error_reply(403, "insufficientFilePermissions"),
            control_reply(vec![descriptor_file("must-not-be-requested")]),
        ]);
        let (transport, test) =
            deps_with_inspection(stored_secret(NOW_MS + 3_600_000));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        let error = provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::Unauthorized);

        let lines = request_lines(&server);
        assert_eq!(lines.len(), 4);
        assert!(lines[2].contains("pageSize=100"));
        assert!(lines[3].contains("pageToken=page-2"));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let first_page = &requests[2];
        let second_page = &requests[3];
        assert_eq!(first_page.authorization, second_page.authorization);
        assert_eq!(
            second_page.authorization.as_deref(),
            Some("Bearer synthetic-access")
        );
        assert_eq!(first_page.account, second_page.account);
        assert_eq!(second_page.account.provider(), "google_drive");
        assert_eq!(second_page.account.principal(), ACCOUNT);
        assert!(!second_page.account.is_pending());
        assert!(second_page.api_request);
    });
}

#[test]
fn listing_rejects_incomplete_first_and_terminal_pages() {
    runtime().block_on(async {
        for body in [
            json!({
                "files": [
                    { "id": "snap-1", "size": "10", "version": "2",
                      "appProperties": { "risunestRole": "state", "risunestObjectId": "s1" } }
                ],
                "incompleteSearch": true,
                "nextPageToken": "page-2"
            }),
            json!({
                "files": [
                    { "id": "snap-2", "size": "12", "version": "3",
                      "appProperties": { "risunestRole": "state", "risunestObjectId": "s2" } }
                ],
                "incompleteSearch": true
            }),
        ] {
            let mut replies = open_existing_replies();
            replies.push(json_reply(200, body));
            let server = WireServer::start(replies);
            let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
            let cancel = Cancellation::default();
            let (provider, repository) = opened(&server, &test, &cancel).await;
            assert_eq!(
                provider
                    .list_objects(&repository, Collection::Snapshots, None, 1, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Corrupt
            );
            let lines = request_lines(&server);
            assert_eq!(lines.len(), 4);
            assert!(lines[3].contains("incompleteSearch"));
        }
    });
}

#[test]
fn incomplete_middle_control_page_exposes_no_cleanup_capable_handle() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            json_reply(
                200,
                json!({
                    "files": [descriptor_file("desc-1")],
                    "incompleteSearch": false,
                    "nextPageToken": "page-2"
                }),
            ),
            json_reply(
                200,
                json!({
                    "files": [],
                    "incompleteSearch": true,
                    "nextPageToken": "page-3"
                }),
            ),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        let error = provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::Corrupt);
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 4);
        assert!(lines[3].contains("pageToken=page-2"));
    });
}

#[test]
fn cleanup_admission_aborts_on_incomplete_drive_listing_without_delete() {
    runtime().block_on(async {
        let mut replies = open_existing_replies();
        replies.push(json_reply(
            200,
            json!({
                "files": [],
                "incompleteSearch": true,
                "nextPageToken": "must-not-be-requested"
            }),
        ));
        replies.push(Reply::Http {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        });
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        let (repository, capabilities) = provider
            .open_repository(
                &connection(server.url.as_str()),
                &secret_ref(),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .unwrap();
        assert!(capabilities.cleanup_supported());

        let directory = tempfile::tempdir().unwrap();
        let descriptor =
            Descriptor::new(repository.repository_id.clone(), Some(Strategy::Sequential)).unwrap();
        let clock = FakeLeaseClock::new(NOW_MS);
        let root_key = [7u8; 32];
        let context = LeaseContext {
            root: directory.path(),
            connection_id: "drive-connection",
            writer_id: "writer",
            descriptor: &descriptor,
            root_key: &root_key,
            provider: provider.as_ref(),
            repository: &repository,
            clock: &clock,
            protection_supported: true,
        };
        let available_time = |_: std::time::Instant| -> Result<bool> { Ok(true) };
        let request = CleanupRequest {
            job_id: "cleanup",
            cleanup_supported: capabilities.cleanup_supported(),
            connection_time: &available_time,
            limits: CleanupLimits::default(),
        };
        let error = cleanup::run(
            &context,
            &request,
            &UnusedCleanupView,
            &UnusedCleanupDocuments,
            &cancel,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ErrorKind::Corrupt);

        let lines = request_lines(&server);
        assert_eq!(lines.len(), 4);
        assert!(lines[3].contains("value%3D%27lease%27"));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("DELETE "))
                .count(),
            0
        );
    });
}

#[test]
fn control_listing_rejects_a_repeated_page_token_cycle() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            about_reply(),
            folder_reply(),
            json_reply(
                200,
                json!({ "files": [], "nextPageToken": "page-2" }),
            ),
            json_reply(
                200,
                json!({ "files": [], "nextPageToken": "page-1" }),
            ),
            json_reply(
                200,
                json!({ "files": [], "nextPageToken": "page-2" }),
            ),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = provider_of(&test.dependencies);
        let cancel = Cancellation::default();
        assert_eq!(
            provider
                .open_repository(
                    &connection(server.url.as_str()),
                    &secret_ref(),
                    OpenMode::Existing,
                    &cancel,
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 5);
        assert!(lines[3].contains("pageToken=page-2"));
        assert!(lines[4].contains("pageToken=page-1"));
    });
}

#[test]
fn cancellation_during_a_body_leaves_the_staging_file_unverified() {
    runtime().block_on(async {
        let mut replies = open_existing_replies();
        replies.push(json_reply(
            200,
            json!({ "id": "pack-slow", "size": "100", "version": "2" }),
        ));
        replies.push(Reply::DelayedBody);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: "pack-slow".to_owned(),
        };
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("slow"), 100).unwrap();
        let read = async {
            let failure = provider
                .read_object(&repository, &locator, None, &mut sink, &cancel)
                .await
                .err()
                .unwrap();
            assert_eq!(failure.kind, ErrorKind::Cancelled);
        };
        let trigger = async {
            while server.requests.lock().unwrap().len() < 5 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel.cancel();
        };
        tokio::time::timeout(
            std::time::Duration::from_millis(2000),
            futures::future::join(read, trigger),
        )
        .await
        .unwrap();
        assert!(!sink.is_verified());
    });
}

#[test]
fn authorization_uses_the_platform_client_and_the_per_file_scope() {
    let config = connection("https://www.googleapis.com");
    let redirect = url::Url::parse("http://127.0.0.1:52001/oauth").unwrap();
    let policy = authorization_policy(&config, "windows", redirect.clone()).unwrap();
    assert_eq!(policy.client_id, client_id("windows"));
    assert_eq!(
        policy.authorize_url.as_str(),
        "https://accounts.google.com/o/oauth2/v2/auth"
    );
    assert_eq!(
        policy.scopes,
        vec!["https://www.googleapis.com/auth/drive.file".to_owned()]
    );
    let mut app_data = connection("https://www.googleapis.com");
    app_data
        .location
        .insert("space".to_owned(), "appDataFolder".to_owned());
    assert_eq!(
        authorization_policy(&app_data, "windows", redirect.clone())
            .unwrap()
            .scopes,
        vec!["https://www.googleapis.com/auth/drive.appdata".to_owned()]
    );
    assert!(authorization_policy(&config, "symbian", redirect).is_err());
    assert!(authorization_policy(
        &config,
        "ios",
        url::Url::parse("http://127.0.0.1:52001/oauth").unwrap()
    )
    .is_err());
    let (ios, callback_scheme) = ios_authorization_policy(&config).unwrap();
    assert_eq!(ios.client_id, client_id("ios"));
    assert_eq!(
        callback_scheme,
        "com.googleusercontent.apps.123456789012-ios"
    );
    assert_eq!(
        ios.redirect_url.as_str(),
        "com.googleusercontent.apps.123456789012-ios:/oauth2redirect"
    );
    let mut malformed_ios = config.clone();
    malformed_ios
        .oauth_profile
        .as_mut()
        .unwrap()
        .platform_client_ids
        .insert(
            "ios".into(),
            "123-bad:scheme.apps.googleusercontent.com".into(),
        );
    assert!(ios_authorization_policy(&malformed_ios).is_err());
    assert!(authorization_policy(
        &config,
        "windows",
        url::Url::parse("https://attacker.invalid/callback").unwrap()
    )
    .is_err());

    let android = android_web_authorization_policy(&config).unwrap();
    assert_eq!(android.client_id, client_id("android"));
    assert_eq!(
        android.redirect_url.as_str(),
        "https://update.rsyumi.workers.dev/oauth/google-drive-callback"
    );
    let mut custom = config.clone();
    custom.location.insert(
        "oauthRedirectUri".into(),
        "https://oauth.example.test/callback".into(),
    );
    assert_eq!(
        android_web_authorization_policy(&custom)
            .unwrap()
            .redirect_url
            .as_str(),
        "https://oauth.example.test/callback"
    );
    custom.location.insert(
        "oauthRedirectUri".into(),
        "https://oauth.example.test/callback?bad=1".into(),
    );
    assert!(android_web_authorization_policy(&custom).is_err());
}

async fn rejected_android_token_info(reply: Reply) -> (ProviderError, String) {
    let server = WireServer::start(vec![reply, about_reply()]);
    let test = deps_with(None);
    let config = connection(server.url.as_str());
    let settings = AuthorizationSettings::parse(&config, "android").unwrap();
    let account = AccountKey::pending(
        super::config::PROVIDER_ID,
        &settings.api("/").unwrap(),
    )
    .unwrap();
    let cancel = Cancellation::default();
    let error = verify_google_grant(
        &test.dependencies,
        settings.token_info_endpoint().unwrap(),
        &settings.client_id,
        &settings.scopes,
        "synthetic-token",
        &account,
        &cancel,
    )
    .await
    .err()
    .unwrap();
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "account lookup must not follow rejection"
    );
    let request = requests[0].headers.clone();
    drop(requests);
    (error, request)
}

#[test]
fn android_grant_binding_fails_before_account_lookup() {
    runtime().block_on(async {
        let required_scope = "https://www.googleapis.com/auth/drive.file";
        let (wrong_client, request) = rejected_android_token_info(json_reply(
            200,
            json!({
                "issued_to": "999999999999-foreign.apps.googleusercontent.com",
                "scope": required_scope
            }),
        ))
        .await;
        assert_eq!(wrong_client.kind, ErrorKind::ReauthRequired);
        assert!(request.starts_with("GET /synthetic/tokeninfo?access_token=synthetic-token"));

        let (missing_scope, _) = rejected_android_token_info(json_reply(
            200,
            json!({
                "issued_to": client_id("android"),
                "scope": "openid"
            }),
        ))
        .await;
        assert_eq!(missing_scope.kind, ErrorKind::ReauthRequired);

        let (throttled, _) = rejected_android_token_info(json_reply_with(
            429,
            &[("Retry-After", "3")],
            json!({ "error": "rate_limit_exceeded" }),
        ))
        .await;
        assert_eq!(throttled.kind, ErrorKind::RateLimited);
    });
}

#[test]
fn a_code_exchange_returns_a_storable_refresh_payload() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            json_reply(
                200,
                json!({
                    "access_token": "granted-access",
                    "refresh_token": "granted-refresh",
                    "expires_in": 3599
                }),
            ),
            about_reply(),
            json_reply(200, json!({ "access_token": "granted-access" })),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS)));
        let cancel = Cancellation::default();
        let grant = || AuthorizationCode {
            code: SecretBytes(zeroize::Zeroizing::new(b"synthetic-code".to_vec())),
            verifier: SecretBytes(zeroize::Zeroizing::new(b"synthetic-verifier".to_vec())),
            client_id: client_id("windows"),
            redirect_url: url::Url::parse("http://127.0.0.1:52001/oauth").unwrap(),
        };
        let config = connection(server.url.as_str());
        let payload = exchange_authorization_code(
            &test.dependencies,
            &config,
            &grant(),
            Some(zeroize::Zeroizing::new("synthetic-client-secret".into())),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(payload.account_id, ACCOUNT);
        let text = String::from_utf8(payload.secret.0.to_vec()).unwrap();
        assert!(text.contains("\"refreshToken\":\"granted-refresh\""));
        assert!(text.contains("\"accessToken\":\"granted-access\""));
        assert!(text.contains("\"clientSecret\":\"synthetic-client-secret\""));
        assert!(text.contains(&format!(
            "\"accessTokenExpiresAtMs\":{}",
            NOW_MS + 3_599_000
        )));
        let records = server.requests.lock().unwrap();
        let body = String::from_utf8(records[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code_verifier=synthetic-verifier"));
        assert!(body.contains("client_secret=synthetic-client-secret"));
        assert!(body.contains(&format!("client_id={}", client_id("windows"))));
        assert!(records[1]
            .headers
            .starts_with("GET /synthetic/drive/v3/about?fields=user"));
        assert!(records[1]
            .headers
            .contains("authorization: Bearer granted-access"));
        drop(records);

        // A grant without a refresh token cannot keep the connection alive.
        assert_eq!(
            exchange_authorization_code(&test.dependencies, &config, &grant(), None, &cancel,)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::ReauthRequired
        );
    });
}

#[test]
fn deleting_checks_the_parent_and_role_of_the_file_before_removing_it() {
    runtime().block_on(async {
        let member = |role: &str, parent: &str| {
            json!({
                "id": "pack-file",
                "parents": [parent],
                "appProperties": { "risunestRole": role, "risunestObjectId": "p1" }
            })
        };
        let mut replies = open_existing_replies();
        replies.push(json_reply(200, member("pack", FOLDER)));
        replies.push(Reply::Http {
            status: 204,
            headers: vec![],
            body: Vec::new(),
        });
        replies.push(error_reply(404, "notFound"));
        replies.push(json_reply(200, member("descriptor", FOLDER)));
        replies.push(json_reply(200, member("pack", "another-folder")));
        replies.push(json_reply(200, member("inventoryPage", FOLDER)));
        replies.push(Reply::Http { status: 204, headers: vec![], body: Vec::new() });
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let locator = |object: &str| RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: object.to_owned(),
        };

        provider
            .delete_object(&repository, &locator("pack-file"), &cancel)
            .await
            .unwrap();
        // A file that is not there is already in the state the caller wanted.
        provider
            .delete_object(&repository, &locator("pack-file"), &cancel)
            .await
            .unwrap();
        // A descriptor, and a file of another folder, are never removed here.
        for _ in 0..2 {
            assert_eq!(
                provider
                    .delete_object(&repository, &locator("pack-file"), &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        for refused in [
            provider.head_locator(&repository).unwrap(),
            locator("not/a/file/id"),
        ] {
            assert_eq!(
                provider
                    .delete_object(&repository, &refused, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }

        provider.delete_object(&repository, &locator("inventory-file"), &cancel).await.unwrap();
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[9], "DELETE /synthetic/drive/v3/files/inventory-file HTTP/1.1");
        assert!(lines[3].contains("fields=id%2Cparents%2CappProperties"));
        assert_eq!(
            lines[4],
            "DELETE /synthetic/drive/v3/files/pack-file HTTP/1.1"
        );
    });
}
