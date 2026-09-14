//! Synthetic wire tests. Every response is scripted by the loopback fixture;
//! no request leaves the process and no real account or token is involved.
use super::api;
use crate::external_storage::{
    contract::*,
    fake::{loopback_dependencies, MemoryVault, TestDependencies},
    providers::Dependencies,
    transfer::{SpoolSink, SpoolSource},
    wire_fixture::{Reply, WireServer},
};
use serde_json::json;
use std::{collections::BTreeMap, sync::atomic::Ordering};

const NOW_MS: u64 = 1_000_000;
const SECRET: &str = "connection-secret";
const TOKEN: &[u8] = br#"{"token":"synthetic-token"}"#;
const PREFIX: &str = "risunest";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn dependencies() -> TestDependencies {
    loopback_dependencies(MemoryVault::with(SECRET, TOKEN), NOW_MS)
}

fn adapter(dependencies: Dependencies) -> std::sync::Arc<dyn Provider> {
    super::create(dependencies).unwrap()
}

fn config(server: &WireServer) -> ConnectionConfig {
    ConnectionConfig {
        provider: "github_releases".into(),
        profile: None,
        endpoint: server.url.to_string(),
        account_id: "synthetic-owner".into(),
        location: BTreeMap::from([
            ("owner".to_owned(), "synthetic-owner".to_owned()),
            ("repo".to_owned(), "synthetic-repo".to_owned()),
            ("tagPrefix".to_owned(), PREFIX.to_owned()),
            ("uploadEndpoint".to_owned(), server.url.to_string()),
        ]),
        oauth_profile: None,
    }
}

fn secret() -> SecretRef {
    SecretRef(SECRET.into())
}

fn reply(status: u16, body: serde_json::Value) -> Reply {
    Reply::Http {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

fn repository_reply(private: bool) -> Reply {
    reply(
        200,
        json!({ "private": private, "permissions": { "push": true } }),
    )
}

fn release(id: u64, tag: &str) -> serde_json::Value {
    json!({ "id": id, "tag_name": tag })
}

fn asset(id: u64, name: &str, size: u64, digest: Option<String>) -> serde_json::Value {
    json!({ "id": id, "name": name, "size": size, "state": "uploaded", "digest": digest })
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", risunest_sync_wire::hash(bytes))
}

fn job_tag(job_id: &str, seq: u32) -> String {
    format!(
        "{PREFIX}-{}-{seq}",
        api::batch_key(ObjectRole::Pack, job_id)
    )
}

fn source(directory: &std::path::Path, name: &str, bytes: &[u8]) -> SpoolSource {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    SpoolSource::verified(&path, bytes.len() as u64, &risunest_sync_wire::hash(bytes)).unwrap()
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
        sha256: risunest_sync_wire::hash(bytes),
    }
}

async fn open(
    provider: &dyn Provider,
    server: &WireServer,
    mode: OpenMode,
) -> Result<RepositoryHandle> {
    let config = config(server);
    let cancel = Cancellation::default();
    provider
        .open_repository(&config, &secret(), mode, &cancel)
        .await
        .map(|(handle, _)| handle)
}

fn method_of(record: &crate::external_storage::wire_fixture::WireRequest) -> &str {
    record.headers.split(' ').next().unwrap_or_default()
}

fn head_line(record: &crate::external_storage::wire_fixture::WireRequest) -> &str {
    record.headers.lines().next().unwrap_or_default()
}

fn has_header(record: &crate::external_storage::wire_fixture::WireRequest, name: &str) -> bool {
    record
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .any(|(header, _)| header.eq_ignore_ascii_case(name))
}

#[test]
fn configuration_and_missing_secrets_are_refused_before_any_request() {
    runtime().block_on(async {
        let server = WireServer::start(Vec::new());
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let cancel = Cancellation::default();
        let cases: Vec<(&str, ConnectionConfig)> = vec![
            ("wrong provider", {
                let mut config = config(&server);
                config.provider = "s3".into();
                config
            }),
            ("plain http host", {
                let mut config = config(&server);
                config.endpoint = "http://synthetic.invalid/api".into();
                config
            }),
            ("plain http upload host", {
                let mut config = config(&server);
                config
                    .location
                    .insert("uploadEndpoint".into(), "http://synthetic.invalid".into());
                config
            }),
            ("missing owner", {
                let mut config = config(&server);
                config.location.remove("owner");
                config
            }),
            ("missing repo", {
                let mut config = config(&server);
                config.location.remove("repo");
                config
            }),
            ("missing tag prefix", {
                let mut config = config(&server);
                config.location.remove("tagPrefix");
                config
            }),
            ("path shaped repo name", {
                let mut config = config(&server);
                config.location.insert("repo".into(), "../escape".into());
                config
            }),
            ("empty account", {
                let mut config = config(&server);
                config.account_id = String::new();
                config
            }),
            ("oauth profile", {
                let mut config = config(&server);
                config.oauth_profile = Some(OAuthProfile {
                    project_id: "synthetic".into(),
                    platform_client_ids: BTreeMap::new(),
                });
                config
            }),
        ];
        for (label, config) in cases {
            let error = provider
                .open_repository(&config, &secret(), OpenMode::Existing, &cancel)
                .await
                .err()
                .unwrap_or_else(|| panic!("{label} accepted"));
            assert_eq!(error.kind, ErrorKind::Unsupported, "{label}");
        }
        let absent = provider
            .open_repository(
                &config(&server),
                &SecretRef("absent".into()),
                OpenMode::Existing,
                &cancel,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(absent.kind, ErrorKind::ReauthRequired);

        let malformed = loopback_dependencies(MemoryVault::with(SECRET, b"{}"), NOW_MS);
        let error = adapter(malformed.dependencies)
            .open_repository(&config(&server), &secret(), OpenMode::Existing, &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::ReauthRequired);
        assert!(server.requests.lock().unwrap().is_empty());
    });
}

#[test]
fn budget_denial_stops_the_connection_before_dispatch() {
    runtime().block_on(async {
        let server = WireServer::start(Vec::new());
        let test = dependencies();
        test.budget.deny.store(true, Ordering::SeqCst);
        let provider = adapter(test.dependencies.clone());
        let error = open(provider.as_ref(), &server, OpenMode::Existing)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::DailyQuotaExhausted);
        assert!(server.requests.lock().unwrap().is_empty());
        assert_eq!(test.budget.reservations.lock().unwrap().len(), 1);
    });
}

#[test]
fn a_public_repository_is_refused_and_an_occupied_root_cannot_be_created() {
    runtime().block_on(async {
        let public = WireServer::start(vec![repository_reply(false)]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        assert_eq!(
            open(provider.as_ref(), &public, OpenMode::Create)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(public.requests.lock().unwrap().len(), 1);

        let occupied = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(1, &format!("{PREFIX}-d-0"))])),
        ]);
        assert_eq!(
            open(provider.as_ref(), &occupied, OpenMode::Create)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );

        let unrelated = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(1, "v1.0.0")])),
        ]);
        let handle = open(provider.as_ref(), &unrelated, OpenMode::Create)
            .await
            .unwrap();
        assert!(handle
            .connection_identity
            .starts_with("github_releases|http://127.0.0.1:"));
        assert!(handle
            .connection_identity
            .ends_with("|synthetic-owner/synthetic-repo|risunest"));

        let no_push = WireServer::start(vec![reply(
            200,
            json!({ "private": true, "permissions": { "push": false } }),
        )]);
        assert_eq!(
            open(provider.as_ref(), &no_push, OpenMode::Create)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unauthorized
        );
    });
}

#[test]
fn existing_needs_the_descriptor_release_and_one_descriptor_asset() {
    runtime().block_on(async {
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());

        let missing = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(1, "v1.0.0")])),
        ]);
        assert_eq!(
            open(provider.as_ref(), &missing, OpenMode::Existing)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );

        let empty = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(7, &format!("{PREFIX}-d-0"))])),
            reply(200, json!([asset(1, "pack-other", 4, None)])),
        ]);
        assert_eq!(
            open(provider.as_ref(), &empty, OpenMode::Existing)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );

        let present = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(7, &format!("{PREFIX}-d-0"))])),
            reply(200, json!([asset(1, "descriptor-root", 4, None)])),
        ]);
        let handle = open(provider.as_ref(), &present, OpenMode::Existing)
            .await
            .unwrap();
        assert_eq!(handle.repository_id, handle.connection_identity);
        let records = present.requests.lock().unwrap();
        assert!(head_line(&records[0]).contains("/repos/synthetic-owner/synthetic-repo "));
        assert!(head_line(&records[1]).contains("/releases?per_page=30&page=1"));
        assert!(head_line(&records[2]).contains("/releases/7/assets?per_page=100&page=1"));
        assert!(records
            .iter()
            .all(|record| has_header(record, "authorization")));
    });
}

#[test]
fn a_created_asset_reports_the_service_digest_and_a_reusable_locator() {
    runtime().block_on(async {
        let bytes = vec![9u8; 3_000];
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(201, release(31, &job_tag("job-1", 0))),
            reply(
                201,
                asset(
                    77,
                    "pack-object-1",
                    bytes.len() as u64,
                    Some(digest_of(&bytes)),
                ),
            ),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), "pack", &bytes);
        let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        let receipt = provider
            .create_object(&handle, &intent, &source, None, &Cancellation::default())
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "31/77");
        assert_eq!(
            receipt.locator.collection.as_deref(),
            Some(job_tag("job-1", 0).as_str())
        );
        assert_eq!(receipt.byte_length, bytes.len() as u64);
        assert!(receipt.complete);
        let checksum = receipt.checksum.unwrap();
        assert_eq!(checksum.algorithm, "sha256");
        assert_eq!(checksum.value, intent.sha256);
        assert!(checksum.provider_verified);

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 4);
        assert!(head_line(&records[2]).starts_with("POST "));
        assert!(String::from_utf8_lossy(&records[2].body).contains("\"draft\":true"));
        assert!(head_line(&records[3]).contains("/releases/31/assets?name=pack-object-1"));
        assert_eq!(records[3].body, bytes);
        assert!(records[3]
            .headers
            .contains("content-type: application/octet-stream"));

        let reservations = test.budget.reservations.lock().unwrap();
        let upload = &reservations.last().unwrap().0;
        let buckets: Vec<&str> = upload.iter().map(|cost| cost.bucket.as_str()).collect();
        assert_eq!(
            buckets,
            vec![
                api::PRIMARY_BUCKET,
                api::POINT_BUCKET,
                api::CONTENT_MINUTE_BUCKET,
                api::CONTENT_HOUR_BUCKET
            ]
        );
        assert!(upload
            .iter()
            .all(|cost| cost.shared_account == "synthetic-owner"));
        assert_eq!(upload[1].units, 5);
    });
}

#[test]
fn a_lost_upload_response_converges_on_the_stored_asset_without_deleting() {
    runtime().block_on(async {
        let bytes = vec![4u8; 512];
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(201, release(20, &job_tag("job-1", 0))),
            Reply::Lost,
            reply(422, json!({})),
            reply(
                200,
                json!([asset(
                    88,
                    "pack-object-1",
                    bytes.len() as u64,
                    Some(digest_of(&bytes))
                )]),
            ),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), "pack", &bytes);
        let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        let cancel = Cancellation::default();
        let lost = provider
            .create_object(&handle, &intent, &source, None, &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(lost.kind, ErrorKind::Transient);
        let receipt = provider
            .create_object(&handle, &intent, &source, None, &cancel)
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "20/88");
        assert!(receipt.complete);
        assert!(receipt.checksum.unwrap().provider_verified);
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 6);
        // The release is reused from the open connection, never recreated.
        assert_eq!(
            records
                .iter()
                .filter(|record| method_of(record) == "POST")
                .count(),
            3
        );
        assert!(records
            .iter()
            .all(|record| method_of(record) != "DELETE" && method_of(record) != "PATCH"));
    });
}

#[test]
fn a_name_conflict_with_different_bytes_is_refused_and_nothing_is_removed() {
    runtime().block_on(async {
        let bytes = vec![1u8; 64];
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(201, release(20, &job_tag("job-1", 0))),
            reply(422, json!({})),
            reply(
                200,
                json!([asset(88, "pack-object-1", 65, Some(digest_of(b"other")))]),
            ),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), "pack", &bytes);
        let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        let error = provider
            .create_object(&handle, &intent, &source, None, &Cancellation::default())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::PreconditionFailed);
        assert_eq!(error.http_status, Some(422));
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 5);
        assert!(records
            .iter()
            .all(|record| method_of(record) != "DELETE" && method_of(record) != "PATCH"));
    });
}

#[test]
fn a_full_release_rolls_the_batch_to_the_next_release() {
    runtime().block_on(async {
        let bytes = vec![2u8; 16];
        let existing: Vec<serde_json::Value> = (0..api::MAX_ASSETS_PER_RELEASE)
            .map(|index| asset(index as u64 + 1, &format!("pack-old-{index}"), 16, None))
            .collect();
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(422, json!({})),
            reply(200, json!([release(40, &job_tag("job-1", 0))])),
            reply(200, serde_json::Value::Array(existing)),
            reply(201, release(41, &job_tag("job-1", 1))),
            reply(201, asset(99, "pack-object-1", 16, Some(digest_of(&bytes)))),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), "pack", &bytes);
        let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        let receipt = provider
            .create_object(&handle, &intent, &source, None, &Cancellation::default())
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "41/99");
        assert_eq!(
            receipt.locator.collection.as_deref(),
            Some(job_tag("job-1", 1).as_str())
        );
        let records = server.requests.lock().unwrap();
        assert!(String::from_utf8_lossy(&records[2].body).contains(&job_tag("job-1", 0)));
        assert!(String::from_utf8_lossy(&records[5].body).contains(&job_tag("job-1", 1)));
        assert!(head_line(&records[6]).contains("/releases/41/assets?name=pack-object-1"));
    });
}

#[test]
fn snapshot_discovery_pages_across_releases_with_a_resumable_cursor() {
    runtime().block_on(async {
        let releases = json!([
            release(51, &job_tag("job-1", 0)),
            release(52, &job_tag("job-2", 0)),
            release(53, &format!("{PREFIX}-d-0")),
        ]);
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(200, releases.clone()),
            reply(
                200,
                json!([
                    asset(1, "snapshot-a", 10, None),
                    asset(2, "pack-a", 20, None),
                    asset(3, "snapshot-b", 30, None)
                ]),
            ),
            reply(200, json!([asset(4, "snapshot-c", 40, None)])),
            reply(200, releases.clone()),
            reply(200, json!([asset(4, "snapshot-c", 40, None)])),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let cancel = Cancellation::default();
        let first = provider
            .list_objects(&handle, Collection::Snapshots, None, 2, &cancel)
            .await
            .unwrap();
        assert_eq!(
            first
                .objects
                .iter()
                .map(|object| object.locator.object.as_str())
                .collect::<Vec<_>>(),
            vec!["51/1", "51/3"]
        );
        assert_eq!(first.objects[0].byte_length, 10);
        assert!(first.objects.iter().all(|object| object.complete));
        let cursor = first.next_cursor.clone().unwrap();
        let second = provider
            .list_objects(&handle, Collection::Snapshots, Some(&cursor), 2, &cancel)
            .await
            .unwrap();
        assert_eq!(
            second
                .objects
                .iter()
                .map(|object| object.locator.object.as_str())
                .collect::<Vec<_>>(),
            vec!["52/4"]
        );
        assert_eq!(second.next_cursor, None);

        for limit in [0u16, 1001] {
            assert_eq!(
                provider
                    .list_objects(&handle, Collection::Snapshots, None, limit, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        assert_eq!(
            provider
                .list_objects(&handle, Collection::Snapshots, Some("1:0"), 2, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(server.requests.lock().unwrap().len(), 7);
    });
}

#[test]
fn a_download_follows_one_redirect_without_the_token() {
    runtime().block_on(async {
        let bytes = vec![6u8; 4096];
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(9, &format!("{PREFIX}-d-0"))])),
            reply(200, json!([asset(3, "descriptor-root", 4, None)])),
            Reply::Http {
                status: 302,
                headers: vec![("Location".into(), "/synthetic/objects/blob".into())],
                body: Vec::new(),
            },
            Reply::Http {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Existing)
            .await
            .unwrap();
        let locator = RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: Some(format!("{PREFIX}-d-0")),
            object: "9/3".into(),
        };
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("body"), 4096).unwrap();
        let receipt = provider
            .read_object(&handle, &locator, None, &mut sink, &Cancellation::default())
            .await
            .unwrap();
        let ReadReceipt::Body(receipt) = receipt else {
            panic!("conditional read is not offered");
        };
        assert_eq!(receipt.byte_length, 4096);
        assert_eq!(receipt.locator, locator);
        assert_eq!(receipt.version, None);
        let checksum = receipt.checksum.unwrap();
        assert_eq!(checksum.value, risunest_sync_wire::hash(&bytes));
        assert!(!checksum.provider_verified);
        assert!(sink.is_verified());

        let records = server.requests.lock().unwrap();
        assert!(head_line(&records[3]).contains("/releases/assets/3"));
        assert!(records[3]
            .headers
            .contains("accept: application/octet-stream"));
        assert!(has_header(&records[3], "authorization"));
        assert!(head_line(&records[4]).contains("/synthetic/objects/blob"));
        assert!(!has_header(&records[4], "authorization"));

        let reservations = test.budget.reservations.lock().unwrap();
        let hop = &reservations.last().unwrap().0;
        assert_eq!(hop.len(), 1);
        assert_eq!(hop[0].bucket, api::ASSET_BODY_BUCKET);
    });
}

#[test]
fn primary_and_secondary_limits_become_rate_limited_with_a_retry_instant() {
    runtime().block_on(async {
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());

        let primary = WireServer::start(vec![Reply::Http {
            status: 403,
            headers: vec![
                ("x-ratelimit-remaining".into(), "0".into()),
                ("x-ratelimit-reset".into(), "1200".into()),
            ],
            body: Vec::new(),
        }]);
        let error = open(provider.as_ref(), &primary, OpenMode::Existing)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::RateLimited);
        assert_eq!(error.http_status, Some(403));
        assert_eq!(error.retry_at_ms, Some(1_200_000));

        let secondary = WireServer::start(vec![Reply::Http {
            status: 403,
            headers: vec![
                ("retry-after".into(), "60".into()),
                ("x-ratelimit-remaining".into(), "4000".into()),
            ],
            body: Vec::new(),
        }]);
        let error = open(provider.as_ref(), &secondary, OpenMode::Existing)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::RateLimited);
        assert_eq!(error.retry_at_ms, Some(NOW_MS + 60_000));

        let forbidden = WireServer::start(vec![Reply::Http {
            status: 403,
            headers: Vec::new(),
            body: Vec::new(),
        }]);
        assert_eq!(
            open(provider.as_ref(), &forbidden, OpenMode::Existing)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unauthorized
        );

        let expired = WireServer::start(vec![Reply::Http {
            status: 401,
            headers: Vec::new(),
            body: Vec::new(),
        }]);
        assert_eq!(
            open(provider.as_ref(), &expired, OpenMode::Existing)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::ReauthRequired
        );

        let absent = WireServer::start(vec![Reply::Http {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        }]);
        assert_eq!(
            open(provider.as_ref(), &absent, OpenMode::Existing)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );
    });
}

#[test]
fn head_publication_is_unsupported_and_sends_nothing() {
    runtime().block_on(async {
        let server = WireServer::start(vec![repository_reply(true), reply(200, json!([]))]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let locator = RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: None,
            object: "1/2".into(),
        };
        let head = HeadBytes::new(vec![1, 2, 3]).unwrap();
        let cancel = Cancellation::default();
        let before = server.requests.lock().unwrap().len();
        for expected in [
            ExpectedHead::Absent,
            ExpectedHead::Exact(VersionToken("1".into())),
        ] {
            assert_eq!(
                provider
                    .compare_exchange_head(&handle, &locator, &expected, &head, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        assert_eq!(
            provider
                .replace_head(&handle, &locator, &head, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(server.requests.lock().unwrap().len(), before);
        let capabilities = super::capabilities();
        assert!(capabilities.require(PublicationStrategy::Cas).is_err());
        assert!(capabilities
            .require(PublicationStrategy::Sequential)
            .is_err());
    });
}

#[test]
fn foreign_handles_and_locators_are_rejected_as_corrupt() {
    runtime().block_on(async {
        let server = WireServer::start(vec![repository_reply(true), reply(200, json!([]))]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let cancel = Cancellation::default();
        let mut sink = SpoolSink::create(&directory.path().join("body"), 16).unwrap();
        let foreign = crate::external_storage::fake::repository();
        assert_eq!(
            provider
                .read_object(
                    &foreign,
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
        for object in ["9", "9/x", "09/3", ""] {
            let locator = RemoteLocator {
                connection_identity: handle.connection_identity.clone(),
                collection: None,
                object: object.into(),
            };
            assert_eq!(
                provider
                    .read_object(&handle, &locator, None, &mut sink, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Corrupt,
                "{object}"
            );
        }
        let other = RemoteLocator {
            connection_identity: "github_releases|https://other.invalid|a/b|c".into(),
            collection: None,
            object: "1/2".into(),
        };
        assert_eq!(
            provider
                .read_object(&handle, &other, None, &mut sink, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        let bytes = vec![3u8; 8];
        let source = source(directory.path(), "pack", &bytes);
        let mut intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        intent.repository_id = "other-repository".into();
        assert_eq!(
            provider
                .create_object(&handle, &intent, &source, None, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        let mut mismatched = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        mismatched.byte_length = 9;
        assert_eq!(
            provider
                .create_object(&handle, &mismatched, &source, None, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        let mut unsafe_id = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        unsafe_id.object_id = "../escape".into();
        assert_eq!(
            provider
                .create_object(&handle, &unsafe_id, &source, None, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    });
}

#[test]
fn there_is_no_upload_session_and_a_foreign_resume_state_is_refused() {
    runtime().block_on(async {
        let server = WireServer::start(vec![repository_reply(true), reply(200, json!([]))]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let bytes = vec![5u8; 32];
        let source = source(directory.path(), "pack", &bytes);
        let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        let cancel = Cancellation::default();
        assert!(provider
            .begin_upload(&handle, &intent, &cancel)
            .await
            .unwrap()
            .is_none());
        let resume = ResumeState {
            sealed_state: SecretRef("foreign".into()),
            confirmed_offset: 0,
            expires_at_ms: None,
        };
        assert_eq!(
            provider
                .create_object(&handle, &intent, &source, Some(&resume), &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    });
}

#[test]
fn reconciliation_reports_the_stored_object_or_a_restart() {
    runtime().block_on(async {
        let bytes = vec![8u8; 128];
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(200, json!([release(60, &job_tag("job-1", 0))])),
            reply(
                200,
                json!([asset(
                    12,
                    "pack-object-1",
                    bytes.len() as u64,
                    Some(digest_of(&bytes))
                )]),
            ),
            reply(200, json!([])),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        let resume = ResumeState {
            sealed_state: SecretRef("unused".into()),
            confirmed_offset: 0,
            expires_at_ms: None,
        };
        let cancel = Cancellation::default();
        let resolution = provider
            .reconcile_upload(&handle, &intent, &resume, &cancel)
            .await
            .unwrap();
        let UploadResolution::Complete(receipt) = resolution else {
            panic!("a stored asset must resolve as complete");
        };
        assert_eq!(receipt.locator.object, "60/12");
        let missing = provider
            .reconcile_upload(&handle, &intent, &resume, &cancel)
            .await
            .unwrap();
        assert!(matches!(missing, UploadResolution::RestartRequired));
    });
}

#[test]
fn cancelling_during_the_body_stops_the_read() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(9, &format!("{PREFIX}-d-0"))])),
            reply(200, json!([asset(3, "descriptor-root", 4, None)])),
            Reply::DelayedBody,
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Existing)
            .await
            .unwrap();
        let locator = RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: None,
            object: "9/3".into(),
        };
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("body"), 4096).unwrap();
        let cancel = Cancellation::default();
        let read = provider.read_object(&handle, &locator, None, &mut sink, &cancel);
        let trigger = async {
            while server.requests.lock().unwrap().len() < 4 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(
            std::time::Duration::from_millis(2_000),
            futures::future::join(read, trigger),
        )
        .await
        .unwrap();
        assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
        assert!(!sink.is_verified());
    });
}

#[test]
fn a_create_receipt_locator_reads_the_same_bytes_back() {
    runtime().block_on(async {
        let bytes = vec![7u8; 1_024];
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(201, release(31, &job_tag("job-1", 0))),
            reply(
                201,
                asset(
                    77,
                    "snapshot-object-1",
                    bytes.len() as u64,
                    Some(digest_of(&bytes)),
                ),
            ),
            Reply::Http {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), "snapshot", &bytes);
        let intent = object_intent(&handle, ObjectRole::Snapshot, "object-1", &bytes);
        let cancel = Cancellation::default();
        let created = provider
            .create_object(&handle, &intent, &source, None, &cancel)
            .await
            .unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("read"), 1_024).unwrap();
        let receipt = provider
            .read_object(&handle, &created.locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        let ReadReceipt::Body(receipt) = receipt else {
            panic!("an immutable asset is always read as a body");
        };
        assert_eq!(receipt.locator, created.locator);
        assert_eq!(receipt.byte_length, created.byte_length);
        assert_eq!(
            receipt.checksum.unwrap().value,
            risunest_sync_wire::hash(&bytes)
        );
        assert!(sink.is_verified());
        let records = server.requests.lock().unwrap();
        assert!(head_line(&records[4]).contains("/releases/assets/77"));
    });
}

#[test]
fn request_cost_reports_documented_weights_and_nothing_for_unused_operations() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(9, &format!("{PREFIX}-d-0"))])),
            reply(200, json!([asset(3, "descriptor-root", 4, None)])),
        ]);
        let provider = adapter(dependencies().dependencies);
        let handle = open(provider.as_ref(), &server, OpenMode::Existing)
            .await
            .unwrap();
        assert!(provider
            .request_cost(
                &crate::external_storage::fake::repository(),
                ProviderOperation::Get
            )
            .is_err());
        for operation in [
            ProviderOperation::Metadata,
            ProviderOperation::List,
            ProviderOperation::DownloadUrl,
            ProviderOperation::ReconcileUpload,
        ] {
            let costs = provider.request_cost(&handle, operation).unwrap();
            assert_eq!(costs.len(), 2, "{operation:?}");
            assert_eq!(costs[0].shared_account, "synthetic-owner");
            assert_eq!(costs[0].bucket, api::PRIMARY_BUCKET);
            assert_eq!(
                costs[0].reset,
                QuotaReset::Rolling {
                    window_ms: 60 * 60 * 1000
                }
            );
            assert_eq!(costs[1].units, 1);
        }
        let create = provider
            .request_cost(&handle, ProviderOperation::Create)
            .unwrap();
        assert_eq!(create.len(), 4);
        assert_eq!(create[1].units, 5);
        assert_eq!(create[2].reset, QuotaReset::Rolling { window_ms: 60_000 });
        let body = provider
            .request_cost(&handle, ProviderOperation::Get)
            .unwrap();
        assert_eq!(body[0].bucket, api::ASSET_BODY_BUCKET);
        assert_eq!(body[0].reset, QuotaReset::Unknown);
        for operation in [
            ProviderOperation::Range,
            ProviderOperation::UploadSession,
            ProviderOperation::UploadChunk,
            ProviderOperation::CompleteUpload,
            ProviderOperation::CompareExchangeHead,
            ProviderOperation::ReplaceHead,
            ProviderOperation::Authenticate,
        ] {
            assert!(
                provider
                    .request_cost(&handle, operation)
                    .unwrap()
                    .is_empty(),
                "{operation:?} is never dispatched"
            );
        }
    });
}

#[test]
fn descriptor_listing_reads_only_the_descriptor_release() {
    runtime().block_on(async {
        let descriptor_tag = format!("{PREFIX}-d-0");
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([release(9, &descriptor_tag)])),
            reply(200, json!([asset(3, "descriptor-root", 4, None)])),
            reply(
                200,
                json!([
                    release(51, &job_tag("job-1", 0)),
                    release(9, &descriptor_tag)
                ]),
            ),
            reply(
                200,
                json!([
                    asset(3, "descriptor-root", 4, None),
                    asset(5, "pack-stray", 9, None)
                ]),
            ),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Existing)
            .await
            .unwrap();
        let page = provider
            .list_objects(
                &handle,
                Collection::Descriptors,
                None,
                10,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].locator.object, "9/3");
        assert_eq!(
            page.objects[0].locator.collection.as_deref(),
            Some(descriptor_tag.as_str())
        );
        assert_eq!(page.next_cursor, None);
        assert_eq!(server.requests.lock().unwrap().len(), 5);
    });
}
