//! Synthetic wire tests. Every response is scripted by the shared loopback
//! fixture; no account, no token and no network of any kind is involved.
use super::{
    auth::{
        android_web_authorization_policy, authorization_policy, exchange_authorization_code,
        ios_authorization_policy, verify_google_grant,
    },
    config::{self, AuthorizationSettings},
    create,
    provider::GoogleDrive,
};
use crate::external_storage::{
    auth::{AuthorizationCode, SecretBytes},
    cleanup::{self, CleanupLimits, CleanupRequest, JobRoots, ObservedRoots, RepositoryView},
    contract::*,
    fake::{loopback_dependencies, with_transport, FakeLeaseClock, MemoryVault, TestDependencies},
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

fn deps_with_inspection(secret: Vec<u8>) -> (Arc<InspectingTransport>, TestDependencies) {
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
const GRANT_VERIFICATION_REQUESTS: usize = cfg!(any(target_os = "android", target_os = "ios")) as usize;

fn grant_verification_replies() -> Vec<Reply> {
    if GRANT_VERIFICATION_REQUESTS == 0 {
        return Vec::new();
    }
    vec![json_reply(200, json!({
        "aud": client_id(config::platform_key()),
        "azp": client_id(config::platform_key()),
        "scope": "https://www.googleapis.com/auth/drive.file"
    }))]
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

#[test]
fn setup_inspects_and_creates_visible_drive_folders() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            json_reply(
                200,
                json!({
                    "id": "picked-folder",
                    "name": "Existing",
                    "mimeType": "application/vnd.google-apps.folder",
                    "trashed": false
                }),
            ),
            json_reply(
                201,
                json!({
                    "id": "created-folder",
                    "name": "Backups",
                    "mimeType": "application/vnd.google-apps.folder",
                    "trashed": false
                }),
            ),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let provider = GoogleDrive::new(test.dependencies);
        let config = connection(server.url.as_str());
        let cancel = Cancellation::default();

        let inspected = provider
            .inspect_setup_folder(&config, &secret_ref(), ACCOUNT, "picked-folder", &cancel)
            .await
            .unwrap();
        assert_eq!(inspected.id, "picked-folder");
        assert_eq!(inspected.name, "Existing");

        let created = provider
            .create_setup_folder(&config, &secret_ref(), ACCOUNT, "Backups", &cancel)
            .await
            .unwrap();
        assert_eq!(created.id, "created-folder");
        assert_eq!(created.name, "Backups");

        let lines = request_lines(&server);
        assert!(lines[0].starts_with("GET /synthetic/drive/v3/files/picked-folder?"));
        assert!(lines[1].starts_with("POST /synthetic/drive/v3/files?"));
        let requests = server.requests.lock().unwrap();
        let body = String::from_utf8(requests[1].body.clone()).unwrap();
        assert!(body.contains("\"name\":\"Backups\""));
        assert!(body.contains("application/vnd.google-apps.folder"));
    });
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

    fn metadata<'a>(&'a self, _: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
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
            Reply::Http { status: 200, headers: vec![], body: vec![3u8; 20] },
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
            sha256: hash(&vec![3u8; 20]),
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
        replies.extend(grant_verification_replies());
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
        assert_eq!(lines.len(), 4 + GRANT_VERIFICATION_REQUESTS);
        assert!(lines[0].starts_with("POST /synthetic/token"));
        let records = server.requests.lock().unwrap();
        let body = String::from_utf8(records[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=synthetic-refresh"));
        assert!(body.contains("client_id="));
        assert!(body.contains("client_secret=synthetic-client-secret"));
        if GRANT_VERIFICATION_REQUESTS != 0 {
            assert!(lines[1].starts_with("GET /synthetic/tokeninfo?access_token=refreshed-access"));
        }
        assert!(records[1 + GRANT_VERIFICATION_REQUESTS]
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
        replies.extend(grant_verification_replies());
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
        assert_eq!(lines.len(), 5 + GRANT_VERIFICATION_REQUESTS);
        assert!(lines[0].starts_with("GET /synthetic/drive/v3/about"));
        assert!(lines[1].starts_with("POST /synthetic/token"));
        if GRANT_VERIFICATION_REQUESTS != 0 {
            assert!(lines[2].starts_with("GET /synthetic/tokeninfo?access_token=second-access"));
        }
        assert!(lines[2 + GRANT_VERIFICATION_REQUESTS].starts_with("GET /synthetic/drive/v3/about"));
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
        let stored = json!({ "id": "pack-generated", "size": "2048", "version": "4",
            "sha256Checksum": hash(&payload), "appProperties": { "risunestRole": "pack", "risunestObjectId": "object-1" } });
        let mut replies = open_existing_replies();
        replies.extend([ids_reply("pack-generated"), Reply::Lost, error_reply(409, "alreadyExists"),
            json_reply(200, stored.clone()), Reply::Http { status: 200, headers: vec![], body: payload.clone() },
            json_reply(200, json!({ "files": [stored.clone()] })), Reply::Http { status: 200, headers: vec![], body: payload.clone() },
            json_reply(200, json!({ "files": [stored] })), Reply::Http { status: 200, headers: vec![], body: vec![4u8; 2048] }]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "pack", &payload);
        let intent = intent(&repository, "object-1", ObjectRole::Pack, &payload);
        let resume = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
        assert_eq!(request_lines(&server).len(), 4);
        assert_eq!(provider.create_object(&repository, &intent, &source, Some(&resume), &cancel).await.unwrap_err().kind, ErrorKind::Transient);
        let receipt = provider.create_object(&repository, &intent, &source, Some(&resume), &cancel).await.unwrap();
        assert_eq!(receipt.locator.object, "pack-generated");
        let receipt = provider.create_object(&repository, &intent, &source, None, &cancel).await.unwrap();
        assert!(receipt.complete);
        assert!(receipt.checksum.unwrap().provider_verified);
        assert_eq!(provider.create_object(&repository, &intent, &source, None, &cancel).await.unwrap_err().kind, ErrorKind::PreconditionFailed);
        let records = server.requests.lock().unwrap();
        assert_eq!(records.iter().filter(|record| record.headers.contains("/files/generateIds")).count(), 1);
        assert_eq!(records[4].body, records[5].body);
        let body = String::from_utf8_lossy(&records[4].body);
        assert!(body.contains("\"id\":\"pack-generated\""));
        assert!(body.contains("\"risunestObjectId\":\"object-1\""));
        assert!(body.contains("\"risunestJobId\":\"job-1\""));
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
        assert_eq!(resume.expires_at_ms, None);
        let sealed = String::from_utf8(
            test.vault
                .contents(&resume.data.secret().unwrap().0)
                .expect("sealed upload state"),
        )
        .unwrap();
        assert!(sealed.contains("\"sessionUri\":null"));
        assert_eq!(request_lines(&server).len(), 4);
        assert!(sealed.contains("pack-session"));

        set_synthetic_session(&test, &resume, &intent, "pack-session", server.url.join("/synthetic/upload/session/one").unwrap().as_str()).await;
        let receipt = provider
            .create_object(&repository, &intent, &source, Some(&resume), &cancel)
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "pack-session");
        assert_eq!(receipt.locator.collection.as_deref(), Some("snapshots"));
        assert!(receipt.complete);
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 6);
        assert!(records[4]
            .headers
            .to_lowercase()
            .contains("content-range: bytes 0-1048575/1048576"));
        assert_eq!(records[4].body.len(), 1024 * 1024);
        assert!(records[5]
            .headers
            .to_lowercase()
            .contains("content-range: bytes 524288-1048575/1048576"));
        assert_eq!(records[5].body.len(), 1024 * 1024 - 524_288);
        assert!(records[4]
            .headers
            .to_lowercase()
            .contains("/synthetic/upload/session/one"));
        drop(records);
    });
}

#[test]
fn an_expired_session_retains_its_file_id_and_a_confirmed_one_completes() {
    runtime().block_on(async {
        let payload = vec![7u8; 4096];
        let stored = json!({ "id": "pack-expired", "size": "4096", "version": "8", "sha256Checksum": hash(&payload),
            "appProperties": { "risunestRole": "pack", "risunestObjectId": "object-3" } });
        let mut replies = open_existing_replies();
        replies.extend([ids_reply("pack-expired"), json_reply(404, json!({})), error_reply(404, "notFound"),
            json_reply(200, stored), Reply::Http { status: 200, headers: vec![], body: payload.clone() },
            json_reply(200, json!({ "id": "pack-expired", "size": "10", "appProperties": { "risunestRole": "pack" } }))]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let intent = intent(&repository, "object-3", ObjectRole::Pack, &payload);
        let resume = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
        let state = SecretBytes(zeroize::Zeroizing::new(json!({ "fileId": "pack-expired",
            "sessionUri": server.url.join("/synthetic/upload/session/expired").unwrap().as_str(), "intent": intent }).to_string().into_bytes()));
        crate::external_storage::auth::SecretVault::replace(test.vault.as_ref(), &resume.data.secret().unwrap(), &state).await.unwrap();
        let UploadResolution::Resumable(updated) = provider.reconcile_upload(&repository, &intent, Some(&resume), &cancel).await.unwrap()
            else { panic!("must preserve allocated identity"); };
        assert_eq!(updated.data.secret().unwrap().0, resume.data.secret().unwrap().0);
        assert_eq!(updated.confirmed_offset, 0);
        assert!(String::from_utf8(test.vault.contents(&updated.data.secret().unwrap().0).unwrap()).unwrap().contains("pack-expired"));
        let UploadResolution::Complete(receipt) = provider.reconcile_upload(&repository, &intent, Some(&updated), &cancel).await.unwrap()
            else { panic!("expected complete"); };
        assert_eq!(receipt.byte_length, 4096);
        assert!(matches!(provider.reconcile_upload(&repository, &intent, Some(&updated), &cancel).await.unwrap(), UploadResolution::Conflict));
        assert_eq!(request_lines(&server).iter().filter(|line| line.contains("/files/generateIds")).count(), 1);
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
        set_synthetic_session(&test, &resume, &intent, "pack-wanted", server.url.join("/synthetic/upload/session/two").unwrap().as_str()).await;
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
fn a_descriptor_replacement_checks_the_file_and_patches_its_media_once() {
    runtime().block_on(async {
        let metadata = |parent: &str, role: &str| {
            json_reply(
                200,
                json!({
                    "id": "desc-1",
                    "parents": [parent],
                    "appProperties": { "risunestRole": role, "risunestObjectId": "d1" }
                }),
            )
        };
        let mut replies = open_existing_replies();
        replies.push(metadata("another-folder", "descriptor"));
        replies.push(metadata(FOLDER, "pack"));
        replies.push(metadata(FOLDER, "descriptor"));
        replies.push(json_reply(200, json!({ "id": "desc-1", "size": "19", "version": "4" })));
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let bytes = HeadBytes::new(b"switched-descriptor".to_vec()).unwrap();
        let at = |collection: Option<&str>, object: &str| RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: collection.map(str::to_owned),
            object: object.to_owned(),
        };
        for refused in [
            provider.head_locator(&repository).unwrap(),
            at(None, "desc-1"),
            at(Some("snapshots"), "desc-1"),
            at(Some("descriptors"), "not/a/file/id"),
        ] {
            let outcome = provider.replace_descriptor(&repository, &refused, &bytes, &cancel).await;
            assert_eq!(outcome.err().unwrap().kind, ErrorKind::Corrupt, "{}", refused.object);
        }
        assert_eq!(request_lines(&server).len(), 3);

        // A file of another folder, then one not marked as the descriptor, is
        // looked up and never written.
        let descriptor = at(Some("descriptors"), "desc-1");
        for _ in 0..2 {
            let outcome = provider.replace_descriptor(&repository, &descriptor, &bytes, &cancel).await;
            assert_eq!(outcome.err().unwrap().kind, ErrorKind::Corrupt);
        }
        let receipt = provider
            .replace_descriptor(&repository, &descriptor, &bytes, &cancel)
            .await
            .unwrap();
        assert_eq!(
            receipt,
            HeadReceipt {
                version: Some(VersionToken("4".to_owned())),
                complete: true,
            }
        );
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 7);
        for line in &lines[3..6] {
            assert_eq!(
                line,
                "GET /synthetic/drive/v3/files/desc-1?fields=id%2Cparents%2CappProperties HTTP/1.1"
            );
        }
        assert!(
            lines[6].starts_with("PATCH /synthetic/upload/drive/v3/files/desc-1?uploadType=media"),
            "{}",
            lines[6]
        );
        assert_eq!(server.requests.lock().unwrap()[6].body, b"switched-descriptor");
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
        replies.push(json_reply(
            200,
            json!({
                "files": [
                    { "id": "checkpoint-1", "size": "9", "version": "1",
                      "appProperties": { "risunestRole": "snapshot", "risunestObjectId": "c1" } }
                ]
            }),
        ));
        let server = WireServer::start(replies);
        let (transport, test) = deps_with_inspection(stored_secret(NOW_MS + 3_600_000));
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
        let checkpoints = provider
            .list_objects(&repository, Collection::Checkpoints, None, 1, &cancel)
            .await
            .unwrap();
        assert_eq!(checkpoints.objects[0].locator.object, "checkpoint-1");
        assert_eq!(
            checkpoints.objects[0].locator.collection.as_deref(),
            Some("snapshots")
        );
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 6);
        assert!(lines[3].contains("pageSize=1"));
        assert!(lines[3].contains("value%3D%27state%27"));
        assert!(lines[3].contains("value%3D%27bundle%27"));
        assert!(!lines[3].contains("value%3D%27snapshot%27"));
        assert!(lines[4].contains("pageToken=page-2"));
        assert!(lines[5].contains("value%3D%27snapshot%27"));
        assert!(!lines[5].contains("value%3D%27state%27"));
        assert!(server.requests.lock().unwrap()[4]
            .headers
            .to_lowercase()
            .contains("authorization: bearer synthetic-access"));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
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
        let (transport, test) = deps_with_inspection(stored_secret(NOW_MS + 3_600_000));
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
            ledger: None,
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
            json_reply(200, json!({ "files": [], "nextPageToken": "page-2" })),
            json_reply(200, json!({ "files": [], "nextPageToken": "page-1" })),
            json_reply(200, json!({ "files": [], "nextPageToken": "page-2" })),
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
    use crate::external_storage::provider_contract::{a_read_cancelled_during_its_body_stops, held_body};
    runtime().block_on(async {
        let (held, _release) = held_body();
        let mut replies = open_existing_replies();
        replies.push(json_reply(
            200,
            json!({ "id": "pack-slow", "size": "100", "version": "2" }),
        ));
        replies.push(held);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: "pack-slow".to_owned(),
        };
        a_read_cancelled_during_its_body_stops(provider.as_ref(), &repository, &locator, &cancel).await;
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
    let account =
        AccountKey::pending(super::config::PROVIDER_ID, &settings.api("/").unwrap()).unwrap();
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
                "aud": "999999999999-foreign.apps.googleusercontent.com",
                "azp": "999999999999-foreign.apps.googleusercontent.com",
                "scope": required_scope
            }),
        ))
        .await;
        assert_eq!(wrong_client.kind, ErrorKind::ReauthRequired);
        assert!(request.starts_with("GET /synthetic/tokeninfo?access_token=synthetic-token"));

        let (missing_scope, _) = rejected_android_token_info(json_reply(
            200,
            json!({
                "aud": client_id("android"),
                "azp": client_id("android"),
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
fn token_info_accepts_the_documented_audience_and_authorized_party() {
    runtime().block_on(async {
        let client = client_id("android");
        let server = WireServer::start(vec![json_reply(200, json!({
            "aud": client,
            "azp": client,
            "scope": "https://www.googleapis.com/auth/drive.file",
            "expires_in": "3599"
        }))]);
        let test = deps_with(None);
        let config = connection(server.url.as_str());
        let settings = AuthorizationSettings::parse(&config, "android").unwrap();
        let account = AccountKey::pending(super::config::PROVIDER_ID, &settings.api("/").unwrap()).unwrap();
        verify_google_grant(
            &test.dependencies, settings.token_info_endpoint().unwrap(), &client,
            &settings.scopes, "synthetic-token", &account, &Cancellation::default(),
        ).await.unwrap();
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    });
}

#[test]
fn token_info_rejects_missing_or_foreign_client_bindings() {
    runtime().block_on(async {
        let client = client_id("android");
        for mut fields in [
            json!({ "aud": "foreign", "azp": client }),
            json!({ "aud": client, "azp": "foreign" }),
            json!({ "azp": client }),
            json!({ "aud": client }),
            json!({ "issued_to": client }),
        ] {
            fields["scope"] = json!("https://www.googleapis.com/auth/drive.file");
            let (error, _) = rejected_android_token_info(json_reply(200, fields)).await;
            assert_eq!(error.kind, ErrorKind::ReauthRequired);
        }
    });
}

#[test]
fn a_code_exchange_returns_a_storable_refresh_payload() {
    runtime().block_on(async {
        let platform_client_id = client_id(config::platform_key());
        let mut replies = vec![
            json_reply(
                200,
                json!({
                    "access_token": "granted-access",
                    "refresh_token": "granted-refresh",
                    "expires_in": 3599
                }),
            ),
        ];
        replies.extend(grant_verification_replies());
        replies.extend([
            about_reply(),
            json_reply(200, json!({ "access_token": "granted-access" })),
        ]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS)));
        let cancel = Cancellation::default();
        let grant = || AuthorizationCode {
            code: SecretBytes(zeroize::Zeroizing::new(b"synthetic-code".to_vec())),
            verifier: SecretBytes(zeroize::Zeroizing::new(b"synthetic-verifier".to_vec())),
            client_id: platform_client_id.clone(),
            redirect_url: url::Url::parse("http://127.0.0.1:52001/oauth").unwrap(),
            picked_file_id: None,
        };
        let config = connection(server.url.as_str());
        let payload = exchange_authorization_code(
            &test.dependencies,
            &config,
            &grant(),
            Some(zeroize::Zeroizing::new("synthetic-client-secret".into())),
            None,
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
        assert!(body.contains(&format!("client_id={platform_client_id}")));
        if GRANT_VERIFICATION_REQUESTS != 0 {
            assert!(records[1].headers.starts_with("GET /synthetic/tokeninfo?access_token=granted-access"));
        }
        assert!(records[1 + GRANT_VERIFICATION_REQUESTS]
            .headers
            .starts_with("GET /synthetic/drive/v3/about?fields=user"));
        assert!(records[1 + GRANT_VERIFICATION_REQUESTS]
            .headers
            .contains("authorization: Bearer granted-access"));
        drop(records);

        // A grant without a refresh token cannot keep the connection alive.
        assert_eq!(
            exchange_authorization_code(&test.dependencies, &config, &grant(), None, None, &cancel,)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::ReauthRequired
        );
    });
}

#[test]
fn a_renewal_without_a_client_secret_reuses_the_secret_of_the_replaced_credential() {
    runtime().block_on(async {
        let saved = stored_secret_with_client_secret(NOW_MS);
        for (typed, vault, sent) in [
            (None, Some(saved.clone()), Some("synthetic-client-secret")),
            (Some("typed-client-secret"), Some(saved.clone()), Some("typed-client-secret")),
            (None, None, None),
            (None, Some(b"unreadable".to_vec()), None),
        ] {
            let mut replies = vec![json_reply(
                200,
                json!({
                    "access_token": "granted-access",
                    "refresh_token": "granted-refresh",
                    "expires_in": 3599
                }),
            )];
            replies.extend(grant_verification_replies());
            replies.push(about_reply());
            let server = WireServer::start(replies);
            let test = deps_with(vault);
            let grant = AuthorizationCode {
                code: SecretBytes(zeroize::Zeroizing::new(b"synthetic-code".to_vec())),
                verifier: SecretBytes(zeroize::Zeroizing::new(b"synthetic-verifier".to_vec())),
                client_id: client_id(config::platform_key()),
                redirect_url: url::Url::parse("http://127.0.0.1:52001/oauth").unwrap(),
                picked_file_id: None,
            };
            let payload = exchange_authorization_code(
                &test.dependencies,
                &connection(server.url.as_str()),
                &grant,
                typed.map(|secret| zeroize::Zeroizing::new(secret.to_owned())),
                Some(&SecretRef(SECRET.to_owned())),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let body = String::from_utf8(server.requests.lock().unwrap()[0].body.clone()).unwrap();
            let stored = String::from_utf8(payload.secret.0.to_vec()).unwrap();
            match sent {
                Some(secret) => {
                    assert!(body.contains(&format!("client_secret={secret}")));
                    assert!(stored.contains(&format!("\"clientSecret\":\"{secret}\"")));
                }
                None => {
                    assert!(!body.contains("client_secret="));
                    assert!(!stored.contains("clientSecret"));
                }
            }
        }
    });
}

#[test]
fn a_rejected_code_exchange_preserves_the_oauth_error_details() {
    runtime().block_on(async {
        let server = WireServer::start(vec![json_reply(
            400,
            json!({
                "error": "invalid_grant",
                "error_description": "synthetic description that must stay native"
            }),
        )]);
        let test = deps_with(Some(stored_secret(NOW_MS)));
        let grant = AuthorizationCode {
            code: SecretBytes(zeroize::Zeroizing::new(b"synthetic-code".to_vec())),
            verifier: SecretBytes(zeroize::Zeroizing::new(b"synthetic-verifier".to_vec())),
            client_id: client_id(config::platform_key()),
            redirect_url: url::Url::parse("http://127.0.0.1:52001/oauth").unwrap(),
            picked_file_id: None,
        };
        let error = match exchange_authorization_code(
            &test.dependencies,
            &connection(server.url.as_str()),
            &grant,
            None,
            None,
            &Cancellation::default(),
        )
        .await
        {
            Ok(_) => panic!("the synthetic token rejection must fail"),
            Err(error) => error,
        };

        assert_eq!(error.kind, ErrorKind::ReauthRequired);
        assert_eq!(error.http_status, Some(400));
        assert_eq!(error.oauth_error.as_deref(), Some("invalid_grant"));
        assert_eq!(
            error.oauth_error_description.as_deref(),
            Some("synthetic description that must stay native")
        );
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({
                "kind": "reauthRequired",
                "httpStatus": 400,
                "retryAtMs": null,
                "oauthError": "invalid_grant",
                "oauthErrorDescription": "synthetic description that must stay native"
            })
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
        replies.push(Reply::Http {
            status: 204,
            headers: vec![],
            body: Vec::new(),
        });
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

        provider
            .delete_object(&repository, &locator("inventory-file"), &cancel)
            .await
            .unwrap();
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 10);
        assert_eq!(
            lines[9],
            "DELETE /synthetic/drive/v3/files/inventory-file HTTP/1.1"
        );
        assert!(lines[3].contains("fields=id%2Cparents%2CappProperties"));
        assert_eq!(
            lines[4],
            "DELETE /synthetic/drive/v3/files/pack-file HTTP/1.1"
        );
    });
}

#[test]
fn transfer_401_refreshes_once_and_replays_only_confirmed_bytes() {
    runtime().block_on(async {
        for repeated in [false, true] {
            let payload = vec![5u8; 1024];
            let mut replies = open_existing_replies();
            replies.push(ids_reply("session-file"));
            replies.push(error_reply(401, "authError"));
            replies.push(json_reply(200, json!({"access_token":"fresh-access","expires_in":3600,"token_type":"Bearer"})));
            replies.extend(grant_verification_replies());
            replies.push(json_reply_with(308, &[("Range", "bytes=0-511")], json!({})));
            replies.push(if repeated { error_reply(401, "authError") } else {
                json_reply(200, json!({"id":"session-file","size":"1024","version":"1","sha256Checksum":hash(&payload)}))
            });
            let server = WireServer::start(replies);
            let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
            let cancel = Cancellation::default();
            let (provider, repository) = opened(&server, &test, &cancel).await;
            let directory = tempfile::tempdir().unwrap();
            let source = spool(directory.path(), "pack", &payload);
            let intent = intent(&repository, "object", ObjectRole::SyncState, &payload);
            let resume = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
            set_synthetic_session(&test, &resume, &intent, "session-file", server.url.join("/synthetic/upload/session/one").unwrap().as_str()).await;
            let result = provider.create_object(&repository, &intent, &source, Some(&resume), &cancel).await;
            if repeated { assert_eq!(result.err().unwrap().kind, ErrorKind::Unauthorized); }
            else { assert_eq!(result.unwrap().byte_length, 1024); }
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 8 + GRANT_VERIFICATION_REQUESTS);
            if GRANT_VERIFICATION_REQUESTS != 0 {
                assert!(requests[6].headers.starts_with("GET /synthetic/tokeninfo?access_token=fresh-access"));
            }
            assert!(requests[6 + GRANT_VERIFICATION_REQUESTS].headers.to_lowercase().contains("content-range: bytes */1024"));
            assert!(requests[7 + GRANT_VERIFICATION_REQUESTS].headers.to_lowercase().contains("content-range: bytes 512-1023/1024"));
            assert_eq!(requests[7 + GRANT_VERIFICATION_REQUESTS].body, payload[512..]);
        }
    });
}

#[test]
fn multipart_and_media_retry_once_after_token_rejection() {
    runtime().block_on(async {
        let payload = b"synthetic-body";
        let file = json!({"id":"generated-file","size":payload.len().to_string(),"version":"1","sha256Checksum":hash(payload)});
        let mut replies = open_existing_replies();
        replies.extend([
            ids_reply("generated-file"), error_reply(401, "authError"),
            json_reply(200, json!({"access_token":"fresh-access","expires_in":3600,"token_type":"Bearer"})),
        ]);
        replies.extend(grant_verification_replies());
        replies.extend([
            json_reply(200, file.clone()), json_reply(200, file), error_reply(401, "authError"),
            json_reply(200, json!({"access_token":"fresh-access-2","expires_in":3600,"token_type":"Bearer"})),
        ]);
        replies.extend(grant_verification_replies());
        replies.extend([
            Reply::Http { status: 200, headers: vec![], body: payload.to_vec() },
        ]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "pack", payload);
        let intent = intent(&repository, "object", ObjectRole::Pack, payload);
        let resume = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
        let receipt = provider.create_object(&repository, &intent, &source, Some(&resume), &cancel).await.unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("read"), payload.len() as u64).unwrap();
        provider.read_object(&repository, &receipt.locator, None, &mut sink, &cancel).await.unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 11 + 2 * GRANT_VERIFICATION_REQUESTS);
        assert!(requests[4].body.windows(payload.len()).any(|bytes| bytes == payload));
        if GRANT_VERIFICATION_REQUESTS != 0 {
            assert!(requests[6].headers.starts_with("GET /synthetic/tokeninfo?access_token=fresh-access"));
            assert!(requests[11].headers.starts_with("GET /synthetic/tokeninfo?access_token=fresh-access-2"));
        }
        assert!(requests[6 + GRANT_VERIFICATION_REQUESTS].body.windows(payload.len()).any(|bytes| bytes == payload));
    });
}

#[test]
fn creation_rejects_crowded_folders_as_occupied() {
    runtime().block_on(async {
        for count in [1, 3, 4] {
            let server = WireServer::start(vec![about_reply(), folder_reply(), control_reply(
                (0..count).map(|index| descriptor_file(&format!("descriptor-{index}"))).collect())]);
            let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
            let provider = provider_of(&test.dependencies);
            let result = provider.open_repository(&connection(server.url.as_str()), &secret_ref(),
                OpenMode::Create, &Cancellation::default()).await;
            assert_eq!(result.err().unwrap().kind, ErrorKind::PreconditionFailed);
            let requests = request_lines(&server);
            assert_eq!(requests.len(), 3);
            assert!(requests[2].contains("pageSize=1"));
        }
    });
}


#[test]
fn longest_sync_names_fit_the_longest_accepted_root() {
    use crate::external_storage::contract::{MAX_SEGMENT_NAME_BYTES, MAX_SNAPSHOT_NAME_BYTES, segment_object_id};
    let root = "r".repeat(256);
    super::validate_sync_root(&root).unwrap();
    let name = segment_object_id("00000000-0000-4000-8000-000000000001", u64::MAX, &"f".repeat(64)).unwrap();
    assert_eq!(name.len(), MAX_SEGMENT_NAME_BYTES);
    let snapshot = "00000000-0000-4000-8000-000000000002";
    assert_eq!(snapshot.len(), MAX_SNAPSHOT_NAME_BYTES);
    // Drive addresses the root by ID, so it consumes no physical name bytes.
    assert_eq!(format!("segments/{name}").len(), 131);
    assert_eq!(format!("snapshots/{snapshot}").len(), 46);
    assert!(super::validate_sync_root(&(root + "r")).is_err());
}

async fn set_synthetic_session(test: &TestDependencies, resume: &ResumeState, intent: &ObjectIntent, file_id: &str, uri: &str) {
    let bytes = SecretBytes(zeroize::Zeroizing::new(json!({ "fileId": file_id, "sessionUri": uri, "intent": intent }).to_string().into_bytes()));
    crate::external_storage::auth::SecretVault::replace(test.vault.as_ref(), &resume.data.secret().unwrap(), &bytes).await.unwrap();
}



#[test]
fn identical_drive_duplicates_are_verified_and_choose_the_smallest_file_id() {
    runtime().block_on(async {
        let bytes = b"sealed-a";
        let name = crate::external_storage::contract::segment_object_id("00000000-0000-4000-8000-000000000001", u64::MAX, &hash(bytes)).unwrap();
        let file = |id: &str| json!({ "id": id, "name": format!("segments/{name}"), "size": "8", "version": "1",
            "appProperties": { "risunestRole": "segment" } });
        let mut replies = open_existing_replies();
        replies.extend([control_reply(vec![file("file-z"), file("file-a")]),
            Reply::Http { status: 200, headers: vec![], body: bytes.to_vec() }, Reply::Http { status: 200, headers: vec![], body: bytes.to_vec() },
            control_reply(vec![file("file-a"), file("file-z")]), Reply::Http { status: 200, headers: vec![], body: bytes.to_vec() },
            Reply::Http { status: 200, headers: vec![], body: b"sealed-b".to_vec() }]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "sealed", bytes);
        let intent = intent(&repository, &name, ObjectRole::Segment, bytes);
        let receipt = provider.create_object(&repository, &intent, &source, None, &cancel).await.unwrap();
        assert_eq!(receipt.locator.object, format!("file-a/{name}"));
        assert_eq!(provider.create_object(&repository, &intent, &source, None, &cancel).await.unwrap_err().kind, ErrorKind::PreconditionFailed);
        let lines = request_lines(&server);
        assert!(lines[3].contains("name+contains"));
        assert!(lines.iter().all(|line| line.starts_with("GET ")));
    });
}

#[test]
fn segment_preallocation_accepts_the_full_hash_name_and_sends_no_body() {
    runtime().block_on(async {
        let bytes = b"sealed-a";
        let name = crate::external_storage::contract::segment_object_id("00000000-0000-4000-8000-000000000001", u64::MAX, &hash(bytes)).unwrap();
        let mut replies = open_existing_replies();
        replies.extend([ids_reply("allocated-file"), Reply::Lost]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let intent = intent(&repository, &name, ObjectRole::Segment, bytes);
        let state = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
        assert_eq!(request_lines(&server).len(), 4);
        assert!(server.requests.lock().unwrap().iter().all(|request| request.body.is_empty()));
        let sealed = String::from_utf8(test.vault.contents(&state.data.secret().unwrap().0).unwrap()).unwrap();
        assert!(sealed.contains("allocated-file"));
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "sealed", bytes);
        assert_eq!(provider.create_object(&repository, &intent, &source, Some(&state), &cancel).await.unwrap_err().kind, ErrorKind::Transient);
        let records = server.requests.lock().unwrap();
        let metadata = String::from_utf8_lossy(&records[4].body);
        assert!(metadata.contains(&format!("segments/{name}")));
        assert!(!metadata.contains("risunestObjectId"));
    });
}


#[test]
fn object_metadata_reads_file_fields_and_never_the_media() {
    runtime().block_on(async {
        let bytes = b"sealed-pack";
        let file = json!({ "id": "pack-file", "size": bytes.len().to_string(), "version": "3", "sha256Checksum": hash(bytes),
            "appProperties": { "risunestRole": "pack", "risunestObjectId": "pack-1" } });
        let mut replies = open_existing_replies();
        replies.extend([control_reply(vec![file]), error_reply(404, "notFound")]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let opened = request_lines(&server).len();
        let intent = intent(&repository, "pack-1", ObjectRole::Pack, bytes);
        let found = provider.lookup_metadata(&repository, &intent, None, &cancel).await.unwrap().unwrap();
        assert_eq!((found.locator.object.as_str(), found.locator.collection.as_deref()), ("pack-file", None));
        assert_eq!(found.byte_length, bytes.len() as u64);
        assert_eq!(found.version, Some(VersionToken("3".into())));
        assert!(found.checksum.clone().unwrap().provider_verified && found.complete);
        assert!(provider.lookup_metadata(&repository, &intent, Some(&found.locator), &cancel).await.unwrap().is_none());
        let lines = request_lines(&server);
        assert_eq!(lines.len(), opened + 2);
        assert!(lines[opened..].iter().all(|line| line.starts_with("GET ") && !line.contains("alt=media")));
        assert!(lines[opened + 1].contains("/files/pack-file?"));
    });
}

#[test]
fn distinct_drive_hash_variants_stop_reconciliation_and_both_remain_listable() {
    runtime().block_on(async {
        let bytes = b"sealed-a";
        let first = crate::external_storage::contract::segment_object_id("00000000-0000-4000-8000-000000000001", 1, &hash(bytes)).unwrap();
        let second = crate::external_storage::contract::segment_object_id("00000000-0000-4000-8000-000000000001", 1, &hash(b"sealed-b")).unwrap();
        let file = |id: &str, name: &str| json!({ "id": id, "name": format!("segments/{name}"), "size": "8", "version": "1",
            "appProperties": { "risunestRole": "segment" } });
        let files = vec![file("file-a", &first), file("file-b", &second)];
        let mut replies = open_existing_replies();
        replies.extend([control_reply(files.clone()), Reply::Http { status: 200, headers: vec![], body: bytes.to_vec() }, control_reply(files),
            control_reply(vec![json!({ "id":"snapshot-file", "size":"8", "version":"1", "appProperties": { "risunestRole":"snapshot" } })])]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let intent = intent(&repository, &first, ObjectRole::Segment, bytes);
        assert_eq!(provider.reconcile_upload(&repository, &intent, None, &cancel).await.err().unwrap().kind, ErrorKind::PreconditionFailed);
        let page = provider.list_objects(&repository, Collection::Segments, None, 10, &cancel).await.unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].locator.object, format!("file-a/{first}"));
        assert_eq!(page.objects[1].locator.object, format!("file-b/{second}"));
        let page = provider.list_objects(&repository, Collection::Checkpoints, None, 10, &cancel).await.unwrap();
        assert_eq!(page.objects[0].locator.object, "snapshot-file");
        assert!(request_lines(&server).iter().all(|line| line.starts_with("GET ")));
    });
}

#[test]
fn segment_listing_keeps_logical_names_and_routes_read_stat_delete_to_file_ids() {
    runtime().block_on(async {
        let writer = "00000000-0000-4000-8000-000000000001";
        let key = [31; 32];
        let segment = crate::external_storage::lww_segment::Segment::new("synthetic-drive-library", writer, u64::MAX);
        let (bytes, _) = crate::external_storage::lww_segment::seal(&segment, &key).unwrap();
        let digest = hash(&bytes);
        let name = segment_object_id(writer, u64::MAX, &digest).unwrap();
        let file = |id: &str| json!({ "id":id, "name":format!("segments/{name}"),
            "size":bytes.len().to_string(), "version":"9", "sha256Checksum":digest,
            "parents":[FOLDER], "appProperties":{"risunestRole":"segment"} });
        let mut replies = open_existing_replies();
        replies.extend([control_reply(vec![file("opaque-file-a"), file("opaque-file-b")]),
            json_reply(200, file("opaque-file-a")), json_reply(200, file("opaque-file-a")),
            Reply::Http {status:200,headers:vec![],body:bytes.clone()},
            json_reply(200, file("opaque-file-a")), Reply::Http {status:204,headers:vec![("content-type".into(), "text/html; charset=UTF-8".into())],body:vec![]}]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let page = provider.list_objects(&repository, Collection::Segments, None, 10, &cancel).await.unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].locator.object, format!("opaque-file-a/{name}"));
        assert_eq!(page.objects[1].locator.object, format!("opaque-file-b/{name}"));
        assert_eq!(page.objects[0].checksum.as_ref().unwrap().value, digest);
        assert!(!page.objects[0].checksum.as_ref().unwrap().provider_verified);
        let (listed_writer, seq, listed_hash) = parse_segment_object_id(page.objects[0].locator.object.rsplit('/').next().unwrap()).unwrap();
        assert_eq!((listed_writer, seq, listed_hash), (writer, u64::MAX, digest.as_str()));
        let directory = tempfile::tempdir().unwrap();
        let mut unchanged = SpoolSink::create(&directory.path().join("unchanged"), bytes.len() as u64).unwrap();
        assert_eq!(provider.read_object(&repository, &page.objects[0].locator, Some(&VersionToken("9".into())), &mut unchanged, &cancel).await.unwrap(),
            ReadReceipt::NotModified(VersionToken("9".into())));
        assert_eq!(request_lines(&server).len(), 5);
        let body_path = directory.path().join("body");
        let mut sink = SpoolSink::create(&body_path, bytes.len() as u64).unwrap();
        let ReadReceipt::Body(receipt) = provider.read_object(&repository, &page.objects[0].locator, None, &mut sink, &cancel).await.unwrap()
            else { panic!("expected a verified segment body") };
        assert_eq!(receipt.locator, page.objects[0].locator);
        assert!(receipt.checksum.unwrap().provider_verified && sink.is_verified());
        let downloaded = std::fs::read(body_path).unwrap();
        let opened_segment = crate::external_storage::lww_segment::open(&downloaded, "synthetic-drive-library", listed_writer, seq, &key).unwrap();
        assert_eq!(opened_segment.encode().unwrap(), segment.encode().unwrap());
        provider.delete_object(&repository, &receipt.locator, &cancel).await.unwrap();
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 9);
        assert!(lines[4].starts_with("GET /synthetic/drive/v3/files/opaque-file-a?fields="));
        assert!(lines[5].starts_with("GET /synthetic/drive/v3/files/opaque-file-a?fields="));
        assert!(lines[6].contains("/files/opaque-file-a?alt=media"));
        assert!(lines[7].starts_with("GET /synthetic/drive/v3/files/opaque-file-a?fields="));
        assert_eq!(lines[8], "DELETE /synthetic/drive/v3/files/opaque-file-a HTTP/1.1");
        assert!(lines[4..].iter().all(|line| !line.contains(&name)));
    });
}

#[test]
fn segment_listing_refuses_missing_malformed_or_foreign_names() {
    runtime().block_on(async {
        let name = segment_object_id("00000000-0000-4000-8000-000000000001", 1, &hash(b"sealed-a")).unwrap();
        for (id, file_name, role) in [
            ("opaque-file", None, "segment"),
            ("opaque-file", Some(format!("snapshots/{name}")), "segment"),
            ("opaque-file", Some(format!("segments/{name}/extra")), "segment"),
            ("opaque-file", Some(format!("segments/{}", name.replacen("-1-", "-01-", 1))), "segment"),
            ("bad/file", Some(format!("segments/{name}")), "segment"),
            ("opaque-file", Some(format!("segments/{name}")), "pack"),
        ] {
            let mut replies = open_existing_replies();
            replies.push(control_reply(vec![json!({"id":id,"name":file_name,"size":"8","appProperties":{"risunestRole":role}})]));
            let server = WireServer::start(replies);
            let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
            let cancel = Cancellation::default();
            let (provider, repository) = opened(&server, &test, &cancel).await;
            assert_eq!(provider.list_objects(&repository, Collection::Segments, None, 10, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
            assert_eq!(request_lines(&server).len(), 4);
        }
    });
}

#[test]
fn segment_locators_reject_raw_ids_and_changed_metadata_before_media_or_delete() {
    runtime().block_on(async {
        let writer = "00000000-0000-4000-8000-000000000001";
        let name = segment_object_id(writer, 1, &hash(b"sealed-a")).unwrap();
        let changed = segment_object_id(writer, 2, &hash(b"sealed-a")).unwrap();
        let file = |id: &str, name: &str| json!({"id":id,"name":format!("segments/{name}"),"size":"8","version":"9",
            "parents":[FOLDER],"appProperties":{"risunestRole":"segment"}});
        let mut replies = open_existing_replies();
        replies.extend([control_reply(vec![file("opaque-file", &name)]),
            json_reply(200, file("opaque-file", &changed)), json_reply(200, file("another-file", &name)),
            json_reply(200, file("opaque-file", &changed))]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let page = provider.list_objects(&repository, Collection::Segments, None, 10, &cancel).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("rejected"), 8).unwrap();
        for object in ["opaque-file".into(), format!("opaque-file/{name}/extra"), format!("bad file/{name}")] {
            let mut locator = page.objects[0].locator.clone(); locator.object = object;
            assert_eq!(provider.read_object(&repository, &locator, None, &mut sink, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
            assert_eq!(provider.delete_object(&repository, &locator, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
        }
        assert_eq!(request_lines(&server).len(), 4);
        assert_eq!(provider.read_object(&repository, &page.objects[0].locator, Some(&VersionToken("9".into())), &mut sink, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
        assert_eq!(provider.read_object(&repository, &page.objects[0].locator, None, &mut sink, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
        assert_eq!(provider.delete_object(&repository, &page.objects[0].locator, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 7);
        assert!(lines.iter().all(|line| line.starts_with("GET ") && !line.contains("alt=media")));
        assert!(!sink.is_verified());
    });
}

#[test]
fn segment_response_loss_reconciles_the_same_preallocated_id_and_logical_name() {
    runtime().block_on(async {
        let bytes = b"sealed-a";
        let name = segment_object_id("00000000-0000-4000-8000-000000000001", 1, &hash(bytes)).unwrap();
        let file = json!({"id":"allocated-segment","name":format!("segments/{name}"),"size":"8","version":"1",
            "sha256Checksum":hash(bytes),"appProperties":{"risunestRole":"segment"}});
        let mut replies = open_existing_replies();
        replies.extend([ids_reply("allocated-segment"), Reply::Lost, json_reply(200, file),
            Reply::Http {status:200,headers:vec![],body:bytes.to_vec()}]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let intent = intent(&repository, &name, ObjectRole::Segment, bytes);
        let resume = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "segment", bytes);
        assert_eq!(provider.create_object(&repository, &intent, &source, Some(&resume), &cancel).await.unwrap_err().kind, ErrorKind::Transient);
        let UploadResolution::Complete(receipt) = provider.reconcile_upload(&repository, &intent, Some(&resume), &cancel).await.unwrap()
            else { panic!("expected the original completed segment") };
        assert_eq!(receipt.locator.object, format!("allocated-segment/{name}"));
        assert_eq!(receipt.locator.collection.as_deref(), Some("segments"));
        assert!(receipt.checksum.unwrap().provider_verified);
        let lines = request_lines(&server);
        assert_eq!(lines.len(), 7);
        assert!(lines[3].contains("generateIds"));
        assert!(lines[4].starts_with("POST "));
        assert!(lines[5].starts_with("GET /synthetic/drive/v3/files/allocated-segment?fields="));
        assert!(lines[6].contains("/files/allocated-segment?alt=media"));
        assert!(lines[5..].iter().all(|line| !line.contains("generateIds") && !line.starts_with("POST ")));
    });
}


#[test]
fn lost_session_initialization_keeps_the_preallocated_id_on_every_retry() {
    runtime().block_on(async {
        let bytes = vec![7u8; 5_000_001];
        let mut replies = open_existing_replies();
        replies.extend([ids_reply("allocated-session"), Reply::Lost, error_reply(404, "notFound"), Reply::Lost]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let directory = tempfile::tempdir().unwrap();
        let source = spool(directory.path(), "sealed", &bytes);
        let intent = intent(&repository, "object", ObjectRole::Pack, &bytes);
        let resume = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
        assert_eq!(provider.create_object(&repository, &intent, &source, Some(&resume), &cancel).await.unwrap_err().kind, ErrorKind::Transient);
        let UploadResolution::Resumable(updated) = provider.reconcile_upload(&repository, &intent, Some(&resume), &cancel).await.unwrap()
            else { panic!("unresolved identity must be retained"); };
        assert_eq!(resume.data.secret().unwrap().0, updated.data.secret().unwrap().0);
        assert_eq!(provider.create_object(&repository, &intent, &source, Some(&updated), &cancel).await.unwrap_err().kind, ErrorKind::Transient);
        let records = server.requests.lock().unwrap();
        assert_eq!(records[4].body, records[6].body);
        assert!(String::from_utf8_lossy(&records[4].body).contains("allocated-session"));
        assert_eq!(records.iter().filter(|record| record.headers.contains("/files/generateIds")).count(), 1);
        assert!(records[4].body.len() < 1024);
    });
}


#[test]
fn retained_drive_session_reports_only_confirmed_offsets_and_verifies_completion() {
    runtime().block_on(async {
        let bytes = vec![7u8; 4096];
        let file = json!({ "id":"fixed-session", "size":"4096", "version":"1", "appProperties":{"risunestRole":"pack"} });
        let mut replies = open_existing_replies();
        replies.extend([ids_reply("fixed-session"), json_reply_with(308, &[("Range", "bytes=0-2047")], json!({})),
            json_reply(200, file), Reply::Http { status:200, headers:vec![], body:bytes.clone() }]);
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let intent = intent(&repository, "object", ObjectRole::Pack, &bytes);
        let resume = provider.begin_upload(&repository, &intent, &cancel).await.unwrap().unwrap();
        set_synthetic_session(&test, &resume, &intent, "fixed-session", server.url.join("/synthetic/upload/session/fixed").unwrap().as_str()).await;
        let UploadResolution::Resumable(updated) = provider.reconcile_upload(&repository, &intent, Some(&resume), &cancel).await.unwrap()
            else { panic!("expected confirmed partial offset"); };
        assert_eq!(updated.confirmed_offset, 2048);
        assert_eq!(updated.data.secret().unwrap().0, resume.data.secret().unwrap().0);
        let UploadResolution::Complete(receipt) = provider.reconcile_upload(&repository, &intent, Some(&updated), &cancel).await.unwrap()
            else { panic!("expected verified completion"); };
        assert_eq!(receipt.checksum.unwrap().value, intent.sha256);
        assert_eq!(receipt.locator.object, "fixed-session");
        let requests = server.requests.lock().unwrap();
        let probes: Vec<_> = requests.iter().filter(|request| request.headers.starts_with("PUT ")).collect();
        assert!(!probes.is_empty());
        for request in probes {
            let headers = request.headers.to_ascii_lowercase();
            assert!(headers.lines().any(|line| line == "content-length: 0"), "empty upload status requests require an explicit length");
            assert!(headers.lines().any(|line| line == "content-range: bytes */4096"));
            assert!(request.body.is_empty());
        }
    });
}

/// The external LWW opening admits its clock from the listing it requests.
#[test]
fn segment_listing_yields_a_usable_clock_sample() {
    runtime().block_on(async {
        let date = httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_millis(NOW_MS));
        let dated = |body: Value| json_reply_with(200, &[("Content-Type", "application/json"), ("Date", &date)], body);
        let server = WireServer::start(vec![
            dated(json!({ "user": { "permissionId": ACCOUNT } })),
            dated(json!({ "id": FOLDER, "mimeType": "application/vnd.google-apps.folder", "trashed": false })),
            dated(json!({ "files": [head_file("head-file", "7"), descriptor_file("desc-1")] })),
            dated(json!({ "files": [] })),
        ]);
        let test = deps_with(Some(stored_secret(NOW_MS + 3_600_000)));
        let cancel = Cancellation::default();
        let (provider, repository) = opened(&server, &test, &cancel).await;
        let started = std::time::Instant::now();
        provider.list_objects(&repository, Collection::Segments, None, 1, &cancel).await.unwrap();
        assert!(test.dependencies.requests.clock_sample_after(&repository.account, started).unwrap().is_some());
        assert!(request_lines(&server).iter().all(|line| line.starts_with("GET ")));
    });
}

#[test]
fn concurrent_expiration_and_rejected_tokens_share_refresh_across_vault_handles() {
    use crate::external_storage::auth::SecretVault;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct VaultHandle(Arc<MemoryVault>);
    impl SecretVault for VaultHandle {
        fn read<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, SecretBytes> { self.0.read(reference) }
        fn store<'a>(&'a self, bytes: &'a SecretBytes) -> ProviderFuture<'a, SecretRef> { self.0.store(bytes) }
        fn replace<'a>(&'a self, reference: &'a SecretRef, bytes: &'a SecretBytes) -> ProviderFuture<'a, ()> { self.0.replace(reference, bytes) }
        fn remove<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, ()> { self.0.remove(reference) }
    }
    struct PausedRefresh {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        calls: AtomicUsize,
        inner: NativeHttpTransport,
    }
    impl HttpTransport for PausedRefresh {
        fn send<'a>(&'a self, request: HttpRequest, cancel: &'a Cancellation) -> ProviderFuture<'a, HttpResponse> {
            Box::pin(async move {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                self.inner.send(request, cancel).await
            })
        }
    }
    runtime().block_on(async {
        for rejected in [None, Some("synthetic-access")] {
            let mut replies = vec![json_reply(200, json!({
                "access_token": "refreshed-access", "expires_in": 3600, "refresh_token": "rotated-refresh"
            }))];
            replies.extend(grant_verification_replies());
            let server = WireServer::start(replies);
            let transport = Arc::new(PausedRefresh {
                entered: Default::default(), release: Default::default(), calls: AtomicUsize::new(0),
                inner: NativeHttpTransport::for_loopback_tests(),
            });
            let expiry = if rejected.is_some() { NOW_MS + 3_600_000 } else { NOW_MS };
            let test = with_transport(transport.clone(), MemoryVault::with(SECRET, &stored_secret(expiry)), NOW_MS);
            let mut left = test.dependencies.clone();
            left.vault = Arc::new(VaultHandle(test.vault.clone()));
            let mut right = test.dependencies.clone();
            right.vault = Arc::new(VaultHandle(test.vault.clone()));
            assert!(!Arc::ptr_eq(&left.vault, &right.vault));
            let settings = config::Settings::parse(&connection(server.url.as_str())).unwrap();
            let account = AccountKey::new("google_drive", &server.url, ACCOUNT).unwrap();
            let first_cache = Mutex::new(None);
            let second_cache = Mutex::new(None);
            let secret = secret_ref();
            let cancel = Cancellation::default();
            let first = super::auth::access_token(&left, &settings, &secret, &account, &first_cache, rejected, &cancel);
            let second = super::auth::access_token(&right, &settings, &secret, &account, &second_cache, rejected, &cancel);
            let controller = async {
                transport.entered.notified().await;
                tokio::task::yield_now().await;
                let before_release = transport.calls.load(Ordering::SeqCst);
                assert!(first_cache.lock().unwrap().is_none());
                assert!(second_cache.lock().unwrap().is_none());
                transport.release.notify_one();
                assert_eq!(before_release, 1);
            };
            let (first, second, ()) = tokio::join!(first, second, controller);
            assert_eq!(first.unwrap().as_str(), "refreshed-access");
            assert_eq!(second.unwrap().as_str(), "refreshed-access");
            assert_eq!(first_cache.lock().unwrap().as_ref().unwrap().access.as_str(), "refreshed-access");
            assert_eq!(second_cache.lock().unwrap().as_ref().unwrap().access.as_str(), "refreshed-access");
            let stored: Value = serde_json::from_slice(&test.vault.contents(SECRET).unwrap()).unwrap();
            assert_eq!(stored["refreshToken"], "rotated-refresh");
            assert_eq!(stored["accessToken"], "refreshed-access");
            assert_eq!(transport.calls.load(Ordering::SeqCst), 1 + GRANT_VERIFICATION_REQUESTS);
        }
    });
}

#[test]
fn failed_rotation_persistence_never_publishes_a_new_cached_token() {
    use crate::external_storage::auth::SecretVault;
    struct FailedReplace(Arc<MemoryVault>);
    impl SecretVault for FailedReplace {
        fn read<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, SecretBytes> { self.0.read(reference) }
        fn store<'a>(&'a self, bytes: &'a SecretBytes) -> ProviderFuture<'a, SecretRef> { self.0.store(bytes) }
        fn replace<'a>(&'a self, _: &'a SecretRef, _: &'a SecretBytes) -> ProviderFuture<'a, ()> {
            Box::pin(async { Err(ProviderError::new(ErrorKind::Transient)) })
        }
        fn remove<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, ()> { self.0.remove(reference) }
    }
    runtime().block_on(async {
        let mut replies = vec![json_reply(200, json!({
            "access_token": "refreshed-access", "expires_in": 3600, "refresh_token": "rotated-refresh"
        }))];
        replies.extend(grant_verification_replies());
        let server = WireServer::start(replies);
        let test = deps_with(Some(stored_secret(NOW_MS)));
        let mut dependencies = test.dependencies.clone();
        dependencies.vault = Arc::new(FailedReplace(test.vault.clone()));
        let settings = config::Settings::parse(&connection(server.url.as_str())).unwrap();
        let account = AccountKey::new("google_drive", &server.url, ACCOUNT).unwrap();
        let cache = Mutex::new(None);
        let error = super::auth::access_token(&dependencies, &settings, &secret_ref(), &account,
            &cache, None, &Cancellation::default()).await.err().unwrap();
        assert_eq!(error.kind, ErrorKind::Transient);
        assert!(cache.lock().unwrap().is_none());
        assert_eq!(test.vault.contents(SECRET).unwrap(), stored_secret(NOW_MS));
    });
}
