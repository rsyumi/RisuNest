//! Synthetic wire tests. No account, network or user data is involved: every
//! response comes from the shared loopback fixture.
use super::create;
use crate::external_storage::{
    capabilities::{Capabilities, Evidence},
    contract::*,
    fake::{loopback_dependencies, MemoryVault, TestDependencies},
    transfer::{SpoolSink, SpoolSource},
    wire_fixture::{Reply, WireServer},
};
use base64::Engine as _;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

const NOW: u64 = 1_000;
const PROJECT: &str = "group/synthetic";
const PACKAGE: &str = "risunest-backup";
const MARKER_BODY: &str = "gitlab_packages|path:group/synthetic|package:risunest-backup";
const TOKEN: &str = "synthetic-token";
const PROJECT_PATH: &str = "/synthetic/api/v4/projects/group%2Fsynthetic";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn hash(bytes: &[u8]) -> String {
    risunest_sync_wire::hash(bytes)
}
fn encoded(object_id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(object_id.as_bytes())
}
fn secret_bytes(kind: &str) -> Vec<u8> {
    format!("{{\"token\":\"{TOKEN}\",\"kind\":\"{kind}\"}}").into_bytes()
}
fn raw(status: u16, bytes: &[u8]) -> Reply {
    Reply::Http {
        status,
        headers: Vec::new(),
        body: bytes.to_vec(),
    }
}
fn json(status: u16, text: &str) -> Reply {
    Reply::Http {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: text.as_bytes().to_vec(),
    }
}
fn headed(status: u16, headers: &[(&str, &str)], text: &str) -> Reply {
    Reply::Http {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: text.as_bytes().to_vec(),
    }
}
fn package_json(id: u64, name: &str, version: &str) -> String {
    format!("{{\"id\":{id},\"name\":\"{name}\",\"version\":\"{version}\"}}")
}
fn file_json(name: &str, size: usize, sha256: Option<String>) -> String {
    let sha256 = match sha256 {
        Some(value) => format!("\"{value}\""),
        None => "null".to_owned(),
    };
    format!(
        "{{\"id\":7,\"package_id\":1,\"file_name\":\"{name}\",\"size\":{size},\"file_sha256\":{sha256}}}"
    )
}
fn marker_present() -> Reply {
    raw(200, MARKER_BODY.as_bytes())
}
fn marker_created() -> Reply {
    json(
        200,
        &file_json(
            "repository",
            MARKER_BODY.len(),
            Some(hash(MARKER_BODY.as_bytes())),
        ),
    )
}
/// Marker probe answered as present plus the listing probe.
fn open_existing_replies() -> Vec<Reply> {
    vec![marker_present(), json(200, "[]")]
}
fn location_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

struct Harness {
    server: WireServer,
    deps: TestDependencies,
    provider: Arc<dyn Provider>,
    secret: SecretRef,
    cancel: Cancellation,
}
fn fixture(replies: Vec<Reply>, kind: &str) -> Harness {
    let server = WireServer::start(replies);
    let deps = loopback_dependencies(MemoryVault::with("gitlab", &secret_bytes(kind)), NOW);
    let provider = create(deps.dependencies.clone()).unwrap();
    Harness {
        server,
        deps,
        provider,
        secret: SecretRef("gitlab".into()),
        cancel: Cancellation::default(),
    }
}
impl Harness {
    fn connection(&self, pairs: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            provider: "gitlab_packages".into(),
            profile: Some("selfManaged".into()),
            endpoint: self.server.url.as_str().to_owned(),
            account_id: "synthetic-user".into(),
            location: location_map(pairs),
            oauth_profile: None,
        }
    }
    fn default_connection(&self) -> ConnectionConfig {
        self.connection(&[("projectId", PROJECT), ("packageName", PACKAGE)])
    }
    async fn open(&self, mode: OpenMode) -> Result<(RepositoryHandle, Capabilities)> {
        let config = self.default_connection();
        self.provider
            .open_repository(&config, &self.secret, mode, &self.cancel)
            .await
    }
    fn lines(&self) -> Vec<String> {
        self.server
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
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }
    fn head(&self, index: usize) -> String {
        self.server.requests.lock().unwrap()[index].headers.clone()
    }
    fn sent(&self, index: usize) -> Vec<u8> {
        self.server.requests.lock().unwrap()[index].body.clone()
    }
    fn reservations(&self) -> Vec<Vec<RequestCost>> {
        self.deps
            .budget
            .reservations
            .lock()
            .unwrap()
            .iter()
            .map(|(costs, _)| costs.clone())
            .collect()
    }
}

fn object_intent(
    repository: &RepositoryHandle,
    role: ObjectRole,
    object_id: &str,
    bytes: &[u8],
) -> ObjectIntent {
    ObjectIntent {
        repository_id: repository.repository_id.clone(),
        job_id: "job-1".into(),
        object_id: object_id.into(),
        role,
        byte_length: bytes.len() as u64,
        sha256: hash(bytes),
    }
}
fn spool_source(directory: &Path, name: &str, bytes: &[u8]) -> SpoolSource {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    SpoolSource::verified(&path, bytes.len() as u64, &hash(bytes)).unwrap()
}
/// `RepositoryHandle` has no Debug formatter, so failures are unwrapped here.
fn failure<T>(result: Result<T>) -> ProviderError {
    match result {
        Ok(_) => panic!("expected a provider error"),
        Err(error) => error,
    }
}
fn resume_state() -> ResumeState {
    ResumeState {
        sealed_state: SecretRef("unused".into()),
        confirmed_offset: 0,
        expires_at_ms: None,
    }
}

#[test]
fn configuration_and_secret_failures_stop_before_any_request() {
    runtime().block_on(async {
        let harness = fixture(Vec::new(), "personalAccessToken");
        let valid = harness.default_connection();
        let mut broken = Vec::new();
        {
            let mut push = |mutate: &dyn Fn(&mut ConnectionConfig)| {
                let mut config = valid.clone();
                mutate(&mut config);
                broken.push(config);
            };
            push(&|config| config.provider = "s3".into());
            push(&|config| config.endpoint = "http://gitlab.example.invalid".into());
            push(&|config| config.endpoint = "https://user:pass@gitlab.example.invalid".into());
            push(&|config| config.account_id = String::new());
            push(&|config| config.profile = Some("gitlabDotCom".into()));
            push(&|config| config.location = location_map(&[("projectId", PROJECT)]));
            push(&|config| {
                config.location = location_map(&[
                    ("projectId", PROJECT),
                    ("packageName", PACKAGE),
                    ("bucket", "no"),
                ]);
            });
            push(&|config| {
                config.location =
                    location_map(&[("projectId", "group%2Fsynthetic"), ("packageName", PACKAGE)]);
            });
            push(&|config| {
                config.location =
                    location_map(&[("projectId", PROJECT), ("packageName", "bad name")]);
            });
            push(&|config| {
                config.location = location_map(&[
                    ("projectId", PROJECT),
                    ("packageName", PACKAGE),
                    ("maxFileBytes", "0"),
                ]);
            });
            push(&|config| {
                config.oauth_profile = Some(OAuthProfile {
                    project_id: "p".into(),
                    platform_client_ids: BTreeMap::new(),
                });
            });
        }
        for config in &broken {
            let error = failure(
                harness
                    .provider
                    .open_repository(config, &harness.secret, OpenMode::Existing, &harness.cancel)
                    .await,
            );
            assert_eq!(error.kind, ErrorKind::Unsupported);
        }

        let missing = loopback_dependencies(MemoryVault::default(), NOW);
        let provider = create(missing.dependencies.clone()).unwrap();
        assert_eq!(
            failure(
                provider
                    .open_repository(
                        &valid,
                        &SecretRef("absent".into()),
                        OpenMode::Existing,
                        &harness.cancel
                    )
                    .await
            )
            .kind,
            ErrorKind::ReauthRequired
        );
        let malformed =
            loopback_dependencies(MemoryVault::with("gitlab", b"{\"token\":\"\"}"), NOW);
        let provider = create(malformed.dependencies.clone()).unwrap();
        assert_eq!(
            failure(
                provider
                    .open_repository(
                        &valid,
                        &SecretRef("gitlab".into()),
                        OpenMode::Existing,
                        &harness.cancel
                    )
                    .await
            )
            .kind,
            ErrorKind::ReauthRequired
        );
        assert!(harness.lines().is_empty());
        assert!(harness.reservations().is_empty());
    });
}

#[test]
fn create_writes_the_root_marker_and_refuses_an_occupied_location() {
    runtime().block_on(async {
        let harness = fixture(
            vec![raw(404, b""), marker_created(), json(200, "[]")],
            "personalAccessToken",
        );
        let (repository, capabilities) = harness.open(OpenMode::Create).await.unwrap();
        assert_eq!(
            repository.connection_identity,
            format!(
                "gitlab_packages|{}|path:{PROJECT}|package:{PACKAGE}",
                harness.server.url.as_str()
            )
        );
        assert_eq!(repository.repository_id, repository.connection_identity);
        assert_eq!(capabilities.immutable_create, Evidence::Synthetic);
        assert_eq!(capabilities.direct_complete_read, Evidence::Synthetic);
        assert_eq!(capabilities.snapshot_discovery, Evidence::Synthetic);
        assert_eq!(capabilities.discovery_extra_requests, 1);
        assert_eq!(capabilities.atomic_create_head, Evidence::Unverified);
        assert_eq!(capabilities.conditional_head_update, Evidence::Unverified);
        assert_eq!(capabilities.stable_head_replace, Evidence::Unverified);
        assert_eq!(capabilities.head_read_after_write, Evidence::Unverified);
        assert_eq!(capabilities.head_retry_control, Evidence::Unverified);
        assert!(!capabilities.conditional_get);
        assert!(!capabilities.range);
        assert!(!capabilities.resumable_upload);
        assert_eq!(capabilities.max_stored_bytes, None);
        assert_eq!(capabilities.documented_at.as_deref(), Some("2026-09-14"));
        assert!(capabilities.evidence_urls.len() >= 4);
        assert!(capabilities.require(PublicationStrategy::Cas).is_err());
        assert!(capabilities.require(PublicationStrategy::Sequential).is_err());

        let lines = harness.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            format!(
                "GET {PROJECT_PATH}/packages/generic/{PACKAGE}.repository/v0/repository HTTP/1.1"
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "PUT {PROJECT_PATH}/packages/generic/{PACKAGE}.repository/v0/repository?select=package_file HTTP/1.1"
            )
        );
        assert_eq!(
            lines[2],
            format!(
                "GET {PROJECT_PATH}/packages?package_type=generic&package_name={PACKAGE}&per_page=1 HTTP/1.1"
            )
        );
        assert_eq!(harness.sent(1), MARKER_BODY.as_bytes());
        assert!(harness.head(1).contains(&format!("private-token: {TOKEN}")));

        let occupied = fixture(vec![marker_present()], "personalAccessToken");
        let error = failure(occupied.open(OpenMode::Create).await);
        assert_eq!(error.kind, ErrorKind::PreconditionFailed);
        assert_eq!(occupied.lines().len(), 1);
    });
}

#[test]
fn existing_requires_the_root_marker_and_rejects_a_foreign_one() {
    runtime().block_on(async {
        let absent = fixture(vec![raw(404, b"")], "personalAccessToken");
        let error = failure(absent.open(OpenMode::Existing).await);
        assert_eq!(error.kind, ErrorKind::NotFound);
        assert_eq!(error.http_status, Some(404));
        assert_eq!(absent.lines().len(), 1);

        let foreign = fixture(vec![raw(200, b"another-repository")], "personalAccessToken");
        assert_eq!(
            failure(foreign.open(OpenMode::Existing).await).kind,
            ErrorKind::Corrupt
        );

        let revoked = fixture(vec![raw(401, b"")], "personalAccessToken");
        assert_eq!(
            failure(revoked.open(OpenMode::Existing).await).kind,
            ErrorKind::ReauthRequired
        );
        let forbidden = fixture(vec![raw(403, b"")], "personalAccessToken");
        assert_eq!(
            failure(forbidden.open(OpenMode::Existing).await).kind,
            ErrorKind::Unauthorized
        );
    });
}

#[test]
fn deploy_token_loses_discovery_but_still_uploads_and_downloads() {
    runtime().block_on(async {
        let payload = b"synthetic-pack-bytes".to_vec();
        let file = format!("pack-{}", encoded("pack-1"));
        let harness = fixture(
            vec![
                marker_present(),
                raw(401, b""),
                raw(404, b""),
                json(200, &file_json(&file, payload.len(), Some(hash(&payload)))),
                raw(200, &payload),
            ],
            "deployToken",
        );
        let (repository, capabilities) = harness.open(OpenMode::Existing).await.unwrap();
        assert_eq!(capabilities.snapshot_discovery, Evidence::Unverified);
        assert_eq!(capabilities.discovery_extra_requests, 0);
        assert_eq!(
            harness
                .provider
                .list_objects(
                    &repository,
                    Collection::Snapshots,
                    None,
                    10,
                    &harness.cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(harness.lines().len(), 2);

        let directory = tempfile::tempdir().unwrap();
        let intent = object_intent(&repository, ObjectRole::Pack, "pack-1", &payload);
        assert!(harness
            .provider
            .begin_upload(&repository, &intent, &harness.cancel)
            .await
            .unwrap()
            .is_none());
        let source = spool_source(directory.path(), "pack", &payload);
        let receipt = harness
            .provider
            .create_object(&repository, &intent, &source, None, &harness.cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.byte_length, payload.len() as u64);
        let checksum = receipt.checksum.clone().unwrap();
        assert_eq!(checksum.algorithm, "sha256");
        assert!(checksum.provider_verified);
        assert_eq!(
            receipt.locator.object,
            format!("{PACKAGE}.pack/v0-{}/{file}", encoded("pack-1"))
        );
        // A token that cannot list still proves absence before it uploads.
        assert_eq!(
            harness.lines()[2],
            format!(
                "GET {PROJECT_PATH}/packages/generic/{PACKAGE}.pack/v0-{}/{file} HTTP/1.1",
                encoded("pack-1")
            )
        );
        assert!(harness.head(3).contains(&format!("deploy-token: {TOKEN}")));
        assert!(!harness.head(3).to_lowercase().contains("private-token"));
        assert_eq!(harness.sent(3), payload);

        let mut sink = SpoolSink::create(&directory.path().join("read"), 4096).unwrap();
        let read = harness
            .provider
            .read_object(
                &repository,
                &receipt.locator,
                None,
                &mut sink,
                &harness.cancel,
            )
            .await
            .unwrap();
        match read {
            ReadReceipt::Body(object) => {
                assert_eq!(object.byte_length, payload.len() as u64);
                assert_eq!(object.checksum.unwrap().value, hash(&payload));
            }
            ReadReceipt::NotModified(_) => panic!("conditional read is not offered"),
        }
        assert!(sink.is_verified());
        assert_eq!(
            std::fs::read(directory.path().join("read")).unwrap(),
            payload
        );
        let lines = harness.lines();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[4], lines[2]);
    });
}

#[test]
fn duplicate_upload_with_matching_bytes_converges_without_resending() {
    runtime().block_on(async {
        let payload = b"catalog-bytes".to_vec();
        let file = format!("catalog-{}", encoded("catalog-1"));
        let version = format!("v0-{}", encoded("catalog-1"));
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                json(
                    200,
                    &format!(
                        "[{}]",
                        package_json(1, &format!("{PACKAGE}.catalog"), &version)
                    ),
                ),
                json(
                    200,
                    &format!(
                        "[{}]",
                        file_json(&file, payload.len(), Some(hash(&payload)))
                    ),
                ),
            ],
            "projectAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let intent = object_intent(&repository, ObjectRole::Catalog, "catalog-1", &payload);
        let source = spool_source(directory.path(), "catalog", &payload);
        let receipt = harness
            .provider
            .create_object(&repository, &intent, &source, None, &harness.cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert!(receipt.checksum.unwrap().provider_verified);
        let lines = harness.lines();
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| !line.starts_with("PUT")));
        assert_eq!(
            lines[2],
            format!(
                "GET {PROJECT_PATH}/packages?package_type=generic&package_name={PACKAGE}.catalog&package_version={version}&per_page=20 HTTP/1.1"
            )
        );
        assert_eq!(
            lines[3],
            format!("GET {PROJECT_PATH}/packages/1/package_files?per_page=100 HTTP/1.1")
        );
    });
}

#[test]
fn duplicate_upload_with_other_bytes_or_two_files_is_a_conflict() {
    runtime().block_on(async {
        let payload = b"snapshot-bytes".to_vec();
        let file = format!("snapshot-{}", encoded("snapshot-1"));
        let version = format!("v0-{}", encoded("snapshot-1"));
        let listing = format!(
            "[{}]",
            package_json(3, &format!("{PACKAGE}.snapshot"), &version)
        );
        let variants = [
            format!(
                "[{}]",
                file_json(&file, payload.len(), Some(hash(b"other")))
            ),
            format!(
                "[{},{}]",
                file_json(&file, payload.len(), Some(hash(&payload))),
                file_json(&file, payload.len(), Some(hash(&payload)))
            ),
            format!("[{}]", file_json(&file, 9_999, Some(hash(&payload)))),
        ];
        for files in variants {
            let harness = fixture(
                vec![
                    marker_present(),
                    json(200, "[]"),
                    json(200, &listing),
                    json(200, &files),
                ],
                "personalAccessToken",
            );
            let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
            let directory = tempfile::tempdir().unwrap();
            let intent = object_intent(&repository, ObjectRole::Snapshot, "snapshot-1", &payload);
            let source = spool_source(directory.path(), "snapshot", &payload);
            assert_eq!(
                harness
                    .provider
                    .create_object(&repository, &intent, &source, None, &harness.cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::PreconditionFailed
            );
            assert_eq!(harness.lines().len(), 4);
        }
    });
}

#[test]
fn a_lost_upload_response_converges_on_retry_with_exactly_one_transfer() {
    runtime().block_on(async {
        let payload = b"pack-after-loss".to_vec();
        let file = format!("pack-{}", encoded("pack-9"));
        let version = format!("v0-{}", encoded("pack-9"));
        let listing = format!(
            "[{}]",
            package_json(5, &format!("{PACKAGE}.pack"), &version)
        );
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                json(200, "[]"),
                Reply::Lost,
                json(200, &listing),
                json(
                    200,
                    &format!(
                        "[{}]",
                        file_json(&file, payload.len(), Some(hash(&payload)))
                    ),
                ),
            ],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let intent = object_intent(&repository, ObjectRole::Pack, "pack-9", &payload);
        let source = spool_source(directory.path(), "pack", &payload);
        assert!(harness
            .provider
            .create_object(&repository, &intent, &source, None, &harness.cancel)
            .await
            .is_err());
        let receipt = harness
            .provider
            .create_object(&repository, &intent, &source, None, &harness.cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        let lines = harness.lines();
        assert_eq!(
            lines.iter().filter(|line| line.starts_with("PUT")).count(),
            1
        );
        assert_eq!(lines.len(), 6);
    });
}

#[test]
fn a_missing_server_digest_falls_back_to_verifying_the_stored_bytes() {
    runtime().block_on(async {
        let payload = b"descriptor-bytes".to_vec();
        let file = format!("descriptor-{}", encoded("descriptor-1"));
        let version = format!("v0-{}", encoded("descriptor-1"));
        let listing = format!(
            "[{}]",
            package_json(8, &format!("{PACKAGE}.descriptor"), &version)
        );
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                json(200, &listing),
                json(200, &format!("[{}]", file_json(&file, payload.len(), None))),
                raw(200, &payload),
            ],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let intent = object_intent(
            &repository,
            ObjectRole::Descriptor,
            "descriptor-1",
            &payload,
        );
        let source = spool_source(directory.path(), "descriptor", &payload);
        let receipt = harness
            .provider
            .create_object(&repository, &intent, &source, None, &harness.cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert!(!receipt.checksum.unwrap().provider_verified);
        assert_eq!(harness.lines().len(), 5);
    });
}

#[test]
fn listing_pages_through_the_next_page_header_and_bounds_its_limit() {
    runtime().block_on(async {
        let snapshots = format!("{PACKAGE}.snapshot");
        let first = format!(
            "[{},{}]",
            package_json(1, &snapshots, &format!("v0-{}", encoded("a"))),
            package_json(2, &snapshots, &format!("v0-{}", encoded("b")))
        );
        let second = format!(
            "[{},{}]",
            package_json(3, &snapshots, &format!("v0-{}", encoded("c"))),
            package_json(4, "unrelated-package", &format!("v0-{}", encoded("d")))
        );
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                headed(200, &[("x-next-page", "2")], &first),
                json(
                    200,
                    &format!(
                        "[{}]",
                        file_json(&format!("snapshot-{}", encoded("a")), 11, Some(hash(b"a")))
                    ),
                ),
                json(
                    200,
                    &format!(
                        "[{}]",
                        file_json(&format!("snapshot-{}", encoded("b")), 22, None)
                    ),
                ),
                headed(200, &[("x-next-page", "")], &second),
                json(
                    200,
                    &format!(
                        "[{}]",
                        file_json(&format!("snapshot-{}", encoded("c")), 33, Some(hash(b"c")))
                    ),
                ),
            ],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let page = harness
            .provider
            .list_objects(&repository, Collection::Snapshots, None, 2, &harness.cancel)
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.next_cursor.as_deref(), Some("2"));
        assert_eq!(
            page.objects[0].locator.object,
            format!(
                "{PACKAGE}.snapshot/v0-{}/snapshot-{}",
                encoded("a"),
                encoded("a")
            )
        );
        assert_eq!(page.objects[0].byte_length, 11);
        assert!(!page.objects[0].checksum.clone().unwrap().provider_verified);
        assert!(page.objects[1].checksum.is_none());
        assert!(page.objects.iter().all(|object| object.complete));

        let last = harness
            .provider
            .list_objects(
                &repository,
                Collection::Snapshots,
                page.next_cursor.as_deref(),
                2,
                &harness.cancel,
            )
            .await
            .unwrap();
        assert_eq!(last.objects.len(), 1);
        assert_eq!(last.next_cursor, None);

        for limit in [0u16, 1001] {
            assert_eq!(
                harness
                    .provider
                    .list_objects(
                        &repository,
                        Collection::Snapshots,
                        None,
                        limit,
                        &harness.cancel
                    )
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        assert_eq!(
            harness
                .provider
                .list_objects(
                    &repository,
                    Collection::Snapshots,
                    Some("not-a-page"),
                    10,
                    &harness.cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        let lines = harness.lines();
        assert_eq!(lines.len(), 7);
        assert_eq!(
            lines[2],
            format!(
                "GET {PROJECT_PATH}/packages?package_type=generic&package_name={PACKAGE}.snapshot&order_by=version&sort=asc&per_page=2 HTTP/1.1"
            )
        );
        assert!(lines[5].contains("&per_page=2&page=2 "));
    });
}

#[test]
fn oversize_objects_and_source_mismatches_never_reach_the_wire() {
    runtime().block_on(async {
        let harness = fixture(open_existing_replies(), "personalAccessToken");
        let config = harness.connection(&[
            ("projectId", PROJECT),
            ("packageName", PACKAGE),
            ("maxFileBytes", "16"),
        ]);
        let (repository, capabilities) = harness
            .provider
            .open_repository(
                &config,
                &harness.secret,
                OpenMode::Existing,
                &harness.cancel,
            )
            .await
            .unwrap();
        assert_eq!(capabilities.max_stored_bytes, Some(16));
        assert_eq!(capabilities.payload_limit(4).unwrap(), Some(12));

        let directory = tempfile::tempdir().unwrap();
        let large = vec![7u8; 32];
        let large_intent = object_intent(&repository, ObjectRole::Pack, "large", &large);
        let large_source = spool_source(directory.path(), "large", &large);
        assert_eq!(
            harness
                .provider
                .create_object(
                    &repository,
                    &large_intent,
                    &large_source,
                    None,
                    &harness.cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::FileTooLarge
        );

        let small = vec![1u8; 8];
        let small_intent = object_intent(&repository, ObjectRole::Pack, "small", &small);
        let shorter = spool_source(directory.path(), "other", &[1u8; 4]);
        assert_eq!(
            harness
                .provider
                .create_object(&repository, &small_intent, &shorter, None, &harness.cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        let small_source = spool_source(directory.path(), "small", &small);
        assert_eq!(
            harness
                .provider
                .create_object(
                    &repository,
                    &small_intent,
                    &small_source,
                    Some(&resume_state()),
                    &harness.cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(harness.lines().len(), 2);
    });
}

#[test]
fn throttling_reports_the_documented_retry_instant() {
    runtime().block_on(async {
        let payload = b"throttled".to_vec();
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                json(200, "[]"),
                headed(429, &[("Retry-After", "60")], ""),
            ],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let intent = object_intent(&repository, ObjectRole::Pack, "throttled", &payload);
        let source = spool_source(directory.path(), "pack", &payload);
        let error = harness
            .provider
            .create_object(&repository, &intent, &source, None, &harness.cancel)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::RateLimited);
        assert_eq!(error.http_status, Some(429));
        assert_eq!(error.retry_at_ms, Some(NOW + 60_000));

        let reset = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                headed(429, &[("RateLimit-Reset", "3600")], ""),
            ],
            "personalAccessToken",
        );
        let (repository, _) = reset.open(OpenMode::Existing).await.unwrap();
        let error = reset
            .provider
            .list_objects(
                &repository,
                Collection::BackupPoints,
                None,
                5,
                &reset.cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::RateLimited);
        assert_eq!(error.retry_at_ms, Some(3_600_000));
    });
}

#[test]
fn reserved_characters_escape_into_documented_version_and_file_names() {
    runtime().block_on(async {
        let object_id = "snapshot/2026 09:14+한글";
        let token = encoded(object_id);
        assert!(token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
        let payload = b"escaped".to_vec();
        let file = format!("snapshot-{token}");
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                json(200, "[]"),
                json(200, &file_json(&file, payload.len(), Some(hash(&payload)))),
                raw(200, &payload),
            ],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let intent = object_intent(&repository, ObjectRole::Snapshot, object_id, &payload);
        let source = spool_source(directory.path(), "snapshot", &payload);
        let receipt = harness
            .provider
            .create_object(&repository, &intent, &source, None, &harness.cancel)
            .await
            .unwrap();
        assert_eq!(
            receipt.locator.object,
            format!("{PACKAGE}.snapshot/v0-{token}/{file}")
        );
        let mut sink = SpoolSink::create(&directory.path().join("out"), 64).unwrap();
        harness
            .provider
            .read_object(
                &repository,
                &receipt.locator,
                None,
                &mut sink,
                &harness.cancel,
            )
            .await
            .unwrap();
        let lines = harness.lines();
        assert_eq!(
            lines[3],
            format!(
                "PUT {PROJECT_PATH}/packages/generic/{PACKAGE}.snapshot/v0-{token}/{file}?select=package_file HTTP/1.1"
            )
        );
        assert_eq!(
            lines[4],
            format!(
                "GET {PROJECT_PATH}/packages/generic/{PACKAGE}.snapshot/v0-{token}/{file} HTTP/1.1"
            )
        );
    });
}

#[test]
fn a_download_redirect_is_followed_once_without_carrying_the_token() {
    runtime().block_on(async {
        let payload = b"redirected-object".to_vec();
        let storage = WireServer::start(vec![raw(200, &payload)]);
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                headed(302, &[("Location", storage.url.as_str())], ""),
            ],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: format!(
                "{PACKAGE}.pack/v0-{}/pack-{}",
                encoded("redirect"),
                encoded("redirect")
            ),
        };
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("out"), 128).unwrap();
        let receipt = harness
            .provider
            .read_object(&repository, &locator, None, &mut sink, &harness.cancel)
            .await
            .unwrap();
        match receipt {
            ReadReceipt::Body(object) => assert_eq!(object.byte_length, payload.len() as u64),
            ReadReceipt::NotModified(_) => panic!("conditional read is not offered"),
        }
        assert!(harness.head(2).contains(&format!("private-token: {TOKEN}")));
        let storage_request = storage.requests.lock().unwrap()[0].headers.to_lowercase();
        assert!(!storage_request.contains("private-token"));
        assert!(!storage_request.contains("deploy-token"));
        // Both hops reserve their own budget.
        assert_eq!(harness.reservations().len(), 4);
    });
}

#[test]
fn foreign_handles_and_locators_are_rejected_and_heads_stay_unsupported() {
    runtime().block_on(async {
        let harness = fixture(open_existing_replies(), "personalAccessToken");
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let good = format!(
            "{PACKAGE}.pack/v0-{}/pack-{}",
            encoded("object"),
            encoded("object")
        );
        let objects = vec![
            "other-package.pack/v0-aa/pack-aa".to_owned(),
            format!("{PACKAGE}.unknown/v0-aa/unknown-aa"),
            format!("{PACKAGE}.pack/v0-aa/pack-bb"),
            format!("{PACKAGE}.pack/plain/pack-aa"),
            format!("{PACKAGE}.pack/v0-aa/pack-aa/extra"),
            good.clone(),
        ];
        for (index, object) in objects.iter().enumerate() {
            let mut sink =
                SpoolSink::create(&directory.path().join(format!("sink-{index}")), 64).unwrap();
            let locator = RemoteLocator {
                // The last case keeps a valid object under a foreign identity.
                connection_identity: if index + 1 == objects.len() {
                    "gitlab_packages|elsewhere".into()
                } else {
                    repository.connection_identity.clone()
                },
                collection: None,
                object: object.clone(),
            };
            assert_eq!(
                harness
                    .provider
                    .read_object(&repository, &locator, None, &mut sink, &harness.cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Corrupt,
                "{object}"
            );
        }

        let head = HeadBytes::new(b"synthetic-head".to_vec()).unwrap();
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: good,
        };
        assert_eq!(
            harness
                .provider
                .compare_exchange_head(
                    &repository,
                    &locator,
                    &ExpectedHead::Absent,
                    &head,
                    &harness.cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(
            harness
                .provider
                .replace_head(&repository, &locator, &head, &harness.cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );

        let foreign = crate::external_storage::fake::repository();
        assert_eq!(
            harness
                .provider
                .replace_head(
                    &foreign,
                    &crate::external_storage::fake::locator(),
                    &head,
                    &harness.cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        // Nothing beyond opening the repository reached the wire.
        assert_eq!(harness.lines().len(), 2);
    });
}

#[test]
fn reconciliation_uses_the_stored_object_rather_than_local_progress() {
    runtime().block_on(async {
        let payload = b"reconciled".to_vec();
        let file = format!("pack-{}", encoded("pack-r"));
        let version = format!("v0-{}", encoded("pack-r"));
        let listing = format!(
            "[{}]",
            package_json(11, &format!("{PACKAGE}.pack"), &version)
        );
        let harness = fixture(
            vec![
                marker_present(),
                json(200, "[]"),
                json(200, &listing),
                json(
                    200,
                    &format!(
                        "[{}]",
                        file_json(&file, payload.len(), Some(hash(&payload)))
                    ),
                ),
                json(200, &listing),
                json(
                    200,
                    &format!(
                        "[{}]",
                        file_json(&file, payload.len(), Some(hash(b"other")))
                    ),
                ),
                json(200, "[]"),
            ],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let intent = object_intent(&repository, ObjectRole::Pack, "pack-r", &payload);
        let state = resume_state();
        match harness
            .provider
            .reconcile_upload(&repository, &intent, &state, &harness.cancel)
            .await
            .unwrap()
        {
            UploadResolution::Complete(receipt) => {
                assert!(receipt.complete);
                assert_eq!(
                    receipt.locator.object,
                    format!("{PACKAGE}.pack/{version}/{file}")
                );
            }
            _ => panic!("expected a completed object"),
        }
        assert!(matches!(
            harness
                .provider
                .reconcile_upload(&repository, &intent, &state, &harness.cancel)
                .await
                .unwrap(),
            UploadResolution::Conflict
        ));
        assert!(matches!(
            harness
                .provider
                .reconcile_upload(&repository, &intent, &state, &harness.cancel)
                .await
                .unwrap(),
            UploadResolution::RestartRequired
        ));
    });
}

#[test]
fn request_costs_name_the_documented_buckets_and_gate_dispatch() {
    runtime().block_on(async {
        let harness = fixture(open_existing_replies(), "personalAccessToken");
        assert!(harness
            .provider
            .request_cost(
                &crate::external_storage::fake::repository(),
                ProviderOperation::Create
            )
            .is_err());
        let (handle, _) = harness.open(OpenMode::Existing).await.unwrap();
        let upload = harness
            .provider
            .request_cost(&handle, ProviderOperation::Create)
            .unwrap();
        assert_eq!(upload.len(), 2);
        assert_eq!(upload[0].bucket, "apiRequests");
        assert_eq!(upload[0].shared_account, "gitlab:user:synthetic-user");
        assert_eq!(upload[0].units, 1);
        assert_eq!(
            upload[0].reset,
            QuotaReset::Rolling {
                window_ms: 60 * 1000
            }
        );
        assert_eq!(upload[1].bucket, "packageRegistryRequests");
        assert_eq!(upload[1].shared_account, "gitlab:address:127.0.0.1");
        assert_eq!(
            harness
                .provider
                .request_cost(&handle, ProviderOperation::List)
                .unwrap()
                .len(),
            1
        );
        let reservations = harness.reservations();
        assert_eq!(reservations.len(), 2);
        assert_eq!(reservations[0].len(), 2);
        assert_eq!(
            reservations[0][0].shared_account,
            "gitlab:user:synthetic-user"
        );
        assert_eq!(
            reservations[0][1].shared_account,
            "gitlab:address:127.0.0.1"
        );
        assert_eq!(reservations[1].len(), 1);

        let denied = fixture(Vec::new(), "personalAccessToken");
        denied.deps.budget.deny.store(true, Ordering::SeqCst);
        assert_eq!(
            failure(denied.open(OpenMode::Existing).await).kind,
            ErrorKind::DailyQuotaExhausted
        );
        assert!(denied.lines().is_empty());
    });
}

#[test]
fn cancelling_an_in_flight_download_stops_the_transfer() {
    runtime().block_on(async {
        let harness = fixture(
            vec![marker_present(), json(200, "[]"), Reply::DelayedHeaders],
            "personalAccessToken",
        );
        let (repository, _) = harness.open(OpenMode::Existing).await.unwrap();
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: format!(
                "{PACKAGE}.pack/v0-{}/pack-{}",
                encoded("slow"),
                encoded("slow")
            ),
        };
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("slow"), 256).unwrap();
        let cancel = harness.cancel.clone();
        let perform = async {
            let error = harness
                .provider
                .read_object(&repository, &locator, None, &mut sink, &cancel)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Cancelled);
        };
        let trigger = async {
            while harness.server.requests.lock().unwrap().len() < 3 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            harness.cancel.cancel();
        };
        tokio::time::timeout(
            Duration::from_millis(3_000),
            futures::future::join(perform, trigger),
        )
        .await
        .unwrap();
        // A cancelled repository refuses further work without a request.
        assert_eq!(
            harness
                .provider
                .list_objects(&repository, Collection::Snapshots, None, 5, &harness.cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Cancelled
        );
        assert_eq!(harness.lines().len(), 3);
    });
}
