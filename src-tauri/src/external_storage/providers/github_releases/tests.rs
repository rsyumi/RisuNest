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
use std::collections::BTreeMap;

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
    config_with_account(
        server,
        &crate::external_storage::quota::credential_principal(
            "github_releases",
            b"synthetic-token",
        ),
    )
}

fn config_with_account(server: &WireServer, account_id: &str) -> ConnectionConfig {
    ConnectionConfig {
        provider: "github_releases".into(),
        profile: None,
        endpoint: server.url.to_string(),
        account_id: account_id.into(),
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

fn github_server(replies: Vec<Reply>) -> WireServer {
    WireServer::start(replies)
}

fn release(id: u64, tag: &str) -> serde_json::Value {
    json!({ "id": id, "tag_name": tag, "draft": true })
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

fn existing_repository_replies() -> Vec<Reply> {
    vec![
        repository_reply(true),
        reply(200, json!([release(7, &format!("{PREFIX}-d-0"))])),
        reply(200, json!([asset(1, "descriptor-root", 4, None)])),
    ]
}

#[test]
fn inventory_lookup_finds_later_releases_after_gaps_and_rejects_duplicates() {
    runtime().block_on(async {
        for duplicate in [false, true] {
            let bytes = b"inventory-payload";
            let mut replies = existing_repository_replies();
            replies.push(reply(200, json!([
                release(40, &job_tag("job-1", 9)),
                release(41, &job_tag("job-1", 12)),
                release(42, &job_tag("other-job", 0)),
            ])));
            replies.push(reply(200, json!([asset(90, "pack-object", bytes.len() as u64, Some(digest_of(bytes)))])));
            replies.push(reply(200, if duplicate {
                json!([asset(91, "pack-object", bytes.len() as u64, Some(digest_of(bytes)))])
            } else { json!([]) }));
            let server = github_server(replies);
            let provider = adapter(dependencies().dependencies);
            let handle = open(provider.as_ref(), &server, OpenMode::Existing).await.unwrap();
            let intent = object_intent(&handle, ObjectRole::Pack, "object", bytes);
            let result = provider.lookup_object(&handle, &intent, &Cancellation::default()).await;
            if duplicate {
                assert_eq!(result.unwrap_err().kind, ErrorKind::PreconditionFailed);
            } else {
                assert_eq!(result.unwrap().unwrap().locator.object, "40/90");
            }
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 6);
            assert!(requests.iter().all(|request| method_of(request) == "GET"));
        }
    });
}

#[test]
fn inventory_collection_lists_only_inventory_assets() {
    runtime().block_on(async {
        let mut replies = existing_repository_replies();
        replies.push(reply(200, json!([release(40, &job_tag("job-1", 9))])));
        replies.push(reply(200, json!([
            asset(90, "inventory-inventory-page-one", 100, None),
            asset(91, "pack-object", 100, None),
        ])));
        let server = github_server(replies);
        let provider = adapter(dependencies().dependencies);
        let handle = open(provider.as_ref(), &server, OpenMode::Existing).await.unwrap();
        let page = provider.list_objects(&handle, Collection::InventoryPages, None, 10, &Cancellation::default()).await.unwrap();
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].locator.object, "40/90");
        assert!(page.next_cursor.is_none());
    });
}

#[test]
fn bounded_lookups_do_not_report_unscanned_objects_as_absent() {
    runtime().block_on(async {
        for kind in ["release", "asset-name", "asset-id"] {
            let pages = if kind == "release" { api::MAX_RELEASE_SCAN_PAGES } else { api::MAX_ASSET_SCAN_PAGES };
            let replies = (0..pages).map(|page| {
                let entries: Vec<_> = if kind == "release" {
                    (0..api::RELEASE_PAGE_SIZE).map(|index| {
                        release(1 + u64::from(page) * 1000 + index as u64, &format!("unrelated-{page}-{index}"))
                    }).collect()
                } else {
                    (0..api::ASSET_PAGE_SIZE).map(|index| {
                        asset(1 + u64::from(page) * 1000 + index as u64, &format!("pack-unrelated-{page}-{index}"), 4, None)
                    }).collect()
                };
                reply(200, json!(entries))
            }).collect();
            let server = github_server(replies);
            let test = dependencies();
            let provider = super::GithubReleases { dependencies: test.dependencies };
            let context = api::Context::new(&config(&server), zeroize::Zeroizing::new("synthetic-token".into())).unwrap();
            let cancel = Cancellation::default();
            let result = match kind {
                "release" => provider.find_release(&context, "unseen-release", &cancel).await.map(|value| value.is_some()),
                "asset-name" => provider.find_asset(&context, 7, "unseen-asset", &cancel).await.map(|value| value.is_some()),
                _ => provider.find_asset_by_id(&context, 7, u64::MAX, &cancel).await.map(|value| value.is_some()),
            };
            assert_eq!(result.unwrap_err().kind, ErrorKind::Transient, "{kind}");
            assert_eq!(server.requests.lock().unwrap().len(), pages as usize);
        }
    });
}

#[test]
fn discovery_returns_a_cursor_when_its_budget_ends_on_a_skipped_release_or_page() {
    runtime().block_on(async {
        for start in [0usize, 29] {
            let tag = job_tag("retained-job", 0);
            let first: Vec<_> = (0..30).map(|id| release(id + 100, "unrelated")).collect();
            let second: Vec<_> = (0..30).map(|id| release(id + 200, "unrelated")).collect();
            let mut third: Vec<_> = (0..30).map(|id| release(id + 300, "unrelated")).collect();
            if start == 0 { third[2] = release(777, &tag); }
            let resumed = if start == 0 { third.clone() } else { vec![release(777, &tag)] };
            let mut replies = existing_repository_replies();
            for page in [first, second, third, resumed] { replies.push(reply(200, json!(page))); }
            replies.push(reply(200, json!([asset(999, &api::asset_name(ObjectRole::BackupBundle, "snapshot-retained"), 4, None)])));
            if start == 0 { replies.push(reply(200, json!([]))); }
            let server = github_server(replies);
            let test = dependencies();
            let provider = adapter(test.dependencies);
            let handle = open(provider.as_ref(), &server, OpenMode::Existing).await.unwrap();
            let cancel = Cancellation::default();
            let cursor = format!("1:{start}:1:0");
            let first = provider.list_objects(&handle, Collection::Snapshots, Some(&cursor), 100, &cancel).await.unwrap();
            assert!(first.objects.is_empty());
            let next = first.next_cursor.expect("A bounded scan has not exhausted the repository");
            assert_ne!(next, cursor);
            let resumed = provider.list_objects(&handle, Collection::Snapshots, Some(&next), 100, &cancel).await.unwrap();
            assert_eq!(resumed.objects.len(), 1);
            assert!(resumed.objects[0].complete);
            assert!(resumed.next_cursor.is_none());
        }
    });
}

#[test]
fn an_exhausted_asset_search_does_not_claim_a_successful_deletion() {
    runtime().block_on(async {
        let tag = job_tag("retained-job", 0);
        let mut replies = existing_repository_replies();
        replies.push(reply(200, release(777, &tag)));
        for page in 0..api::MAX_ASSET_SCAN_PAGES {
            replies.push(reply(200, json!((0..api::ASSET_PAGE_SIZE).map(|index| {
                asset(1 + u64::from(page) * 1000 + index as u64, "pack-unrelated", 4, None)
            }).collect::<Vec<_>>())));
        }
        let server = github_server(replies);
        let test = dependencies();
        let provider = super::GithubReleases { dependencies: test.dependencies };
        let handle = open(&provider, &server, OpenMode::Existing).await.unwrap();
        let locator = provider.context(&handle).unwrap().locator(&tag, 777, u64::MAX);
        assert_eq!(provider.delete_object(&handle, &locator, &Cancellation::default()).await.unwrap_err().kind,
            ErrorKind::Transient);
        assert!(server.requests.lock().unwrap().iter().all(|request| method_of(request) == "GET"));
    });
}

#[test]
fn backup_only_cleanup_reads_points_without_requesting_an_unsupported_head() {
    runtime().block_on(async {
        use crate::external_storage::{cleanup, connection::RetentionPolicy, connection_commands::ConnectedRepository, connection_store::StoredConnection};
        let mut replies = existing_repository_replies();
        replies.push(reply(200, json!([])));
        let server = github_server(replies);
        let test = dependencies();
        let provider = std::sync::Arc::new(super::GithubReleases { dependencies: test.dependencies.clone() });
        let handle = open(provider.as_ref(), &server, OpenMode::Existing).await.unwrap();
        let locator = provider.context(&handle).unwrap().locator(&format!("{PREFIX}-d-0"), 7, 1);
        let connected = ConnectedRepository {
            stored: StoredConnection {
                id: "connection".into(), config: config(&server),
                descriptor: risunest_external_storage_format::format::Descriptor::new("repository".into(), None).unwrap(),
                descriptor_locator: locator, provider_repository_id: handle.repository_id.clone(),
                credential_ref: SECRET.into(), root_key_ref: "synthetic-root-key".into(),
                recovery_key_ref: "synthetic-recovery-key".into(),
                capabilities: super::capabilities(), created_at_ms: NOW_MS,
                last_sync_at_ms: None, last_backup_at_ms: None, capture_policy: None, retention_policy: None,
            },
            provider, handle, dependencies: test.dependencies,
            root_key: zeroize::Zeroizing::new([7; 32]),
        };
        let directory = tempfile::tempdir().unwrap();
        let view = cleanup::ConnectedRepositoryView {
            connected: &connected, writer_id: "writer", policy: RetentionPolicy::DEFAULT,
            now_ms: NOW_MS, unfinished: vec![], cache_root: directory.path(),
        };
        let roots = cleanup::RepositoryView::roots(&view, &Cancellation::default()).await.unwrap();
        assert!(roots.head.is_none());
        assert!(roots.points.is_empty());
        assert_eq!(server.requests.lock().unwrap().len(), 4);
    });
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
fn credential_principal_shares_only_matching_tokens_at_the_same_authority() {
    runtime().block_on(async {
        let server = WireServer::start(vec![
            repository_reply(true),
            reply(200, json!([])),
            repository_reply(true),
            reply(200, json!([])),
            repository_reply(true),
            reply(200, json!([])),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let cancel = Cancellation::default();
        let same = crate::external_storage::quota::credential_principal(
            "github_releases",
            b"synthetic-token",
        );
        let different = crate::external_storage::quota::credential_principal(
            "github_releases",
            b"different-token",
        );
        let different_secret = test.dependencies.vault.store(
            &crate::external_storage::auth::SecretBytes(zeroize::Zeroizing::new(
                br#"{"token":"different-token"}"#.to_vec(),
            )),
        ).await.unwrap();
        let first = provider
            .open_repository(
                &config_with_account(&server, &same),
                &secret(),
                OpenMode::Create,
                &cancel,
            )
            .await
            .unwrap()
            .0;
        let second = provider
            .open_repository(
                &config_with_account(&server, &same),
                &secret(),
                OpenMode::Create,
                &cancel,
            )
            .await
            .unwrap()
            .0;
        let third = provider
            .open_repository(
                &config_with_account(&server, &different),
                &different_secret,
                OpenMode::Create,
                &cancel,
            )
            .await
            .unwrap()
            .0;

        assert_eq!(first.account, second.account);
        assert_ne!(first.account, third.account);
        assert_eq!(first.account.provider(), "github_releases");
        assert_eq!(first.account.principal(), same);
        assert!(!first.account.is_pending());
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 6);
        for pair in records.chunks_exact(2) {
            assert!(head_line(&pair[0]).contains("/repos/synthetic-owner/synthetic-repo "));
            assert!(head_line(&pair[1]).contains("/repos/synthetic-owner/synthetic-repo/releases?"));
        }
        assert!(records[..4]
            .iter()
            .all(|record| record.headers.contains("authorization: Bearer synthetic-token")));
        assert!(records[4..]
            .iter()
            .all(|record| record.headers.contains("authorization: Bearer different-token")));
    });
}

#[test]
fn missing_credential_principal_stops_before_repository_requests() {
    runtime().block_on(async {
        let server = WireServer::start(Vec::new());
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let cancel = Cancellation::default();
        let error = provider
            .open_repository(
                &config_with_account(&server, ""),
                &secret(),
                OpenMode::Create,
                &cancel,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert!(server.requests.lock().unwrap().is_empty());
    });
}

#[test]
fn resume_create_accepts_empty_or_one_exact_descriptor_and_converges() {
    runtime().block_on(async {
        let empty = github_server(vec![
            repository_reply(true),
            reply(200, json!([release(1, "v1.0.0")])),
        ]);
        let provider = adapter(dependencies().dependencies);
        let empty_handle = open(provider.as_ref(), &empty, OpenMode::ResumeCreate)
            .await
            .unwrap();
        assert_eq!(
            empty_handle.account.principal(),
            crate::external_storage::quota::credential_principal(
                "github_releases",
                b"synthetic-token",
            )
        );
        let records = empty.requests.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| method_of(record) == "GET"));
        drop(records);

        let descriptor_tag = format!("{PREFIX}-d-0");
        let released = github_server(vec![
            repository_reply(true),
            reply(200, json!([release(7, &descriptor_tag)])),
            reply(200, json!([])),
        ]);
        let provider = adapter(dependencies().dependencies);
        open(provider.as_ref(), &released, OpenMode::ResumeCreate)
            .await
            .unwrap();
        let records = released.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|record| method_of(record) == "GET"));
        drop(records);

        let bytes = b"root".to_vec();
        let stored = asset(
            3,
            "descriptor-root",
            bytes.len() as u64,
            Some(digest_of(&bytes)),
        );
        let server = github_server(vec![
            repository_reply(true),
            reply(200, json!([release(7, &descriptor_tag)])),
            reply(200, json!([stored.clone()])),
            reply(422, json!({})),
            reply(200, json!([stored])),
            Reply::Http {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::ResumeCreate)
            .await
            .unwrap();
        {
            let records = server.requests.lock().unwrap();
            assert_eq!(records.len(), 3);
            assert!(records.iter().all(|record| method_of(record) == "GET"));
        }

        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), "descriptor", &bytes);
        let intent = object_intent(&handle, ObjectRole::Descriptor, "root", &bytes);
        let receipt = provider
            .create_object(
                &handle,
                &intent,
                &source,
                None,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "7/3");
        let mut sink = SpoolSink::create(&directory.path().join("read"), 16).unwrap();
        provider
            .read_object(
                &handle,
                &receipt.locator,
                None,
                &mut sink,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert!(sink.is_verified());
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 6);
        assert_eq!(method_of(&records[3]), "POST");
        assert!(head_line(&records[3]).contains("/releases/7/assets?name=descriptor-root"));
        assert!(records
            .iter()
            .all(|record| method_of(record) != "PATCH" && method_of(record) != "DELETE"));
    });
}

#[test]
fn resume_create_rejects_head_duplicate_malformed_and_foreign_owned_layouts() {
    runtime().block_on(async {
        let descriptor_tag = format!("{PREFIX}-d-0");
        let cases: Vec<(&str, Vec<Reply>)> = vec![
            (
                "provider-owned head tag",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(2, &format!("{PREFIX}-head-0"))])),
                ],
            ),
            (
                "foreign provider-owned tag",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(2, &format!("{PREFIX}-foreign-0"))])),
                ],
            ),
            (
                "duplicate descriptor releases",
                vec![
                    repository_reply(true),
                    reply(
                        200,
                        json!([
                            release(7, &descriptor_tag),
                            release(8, &descriptor_tag)
                        ]),
                    ),
                ],
            ),
            (
                "head asset",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(7, &descriptor_tag)])),
                    reply(200, json!([asset(1, "head", 4, None)])),
                ],
            ),
            (
                "too many descriptor assets",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(7, &descriptor_tag)])),
                    reply(
                        200,
                        json!([
                            asset(1, "descriptor-one", 4, None),
                            asset(2, "descriptor-two", 4, None),
                            asset(3, "descriptor-three", 4, None)
                        ]),
                    ),
                ],
            ),
            (
                "malformed descriptor asset",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(7, &descriptor_tag)])),
                    reply(200, json!([asset(1, "descriptor-../escape", 4, None)])),
                ],
            ),
            (
                "unfinished descriptor asset",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(7, &descriptor_tag)])),
                    reply(
                        200,
                        json!([{
                            "id": 1,
                            "name": "descriptor-root",
                            "size": 4,
                            "state": "new"
                        }]),
                    ),
                ],
            ),
            (
                "empty descriptor asset",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(7, &descriptor_tag)])),
                    reply(200, json!([asset(1, "descriptor-root", 0, None)])),
                ],
            ),
            (
                "malformed descriptor digest",
                vec![
                    repository_reply(true),
                    reply(200, json!([release(7, &descriptor_tag)])),
                    reply(
                        200,
                        json!([asset(
                            1,
                            "descriptor-root",
                            4,
                            Some("md5:synthetic".into())
                        )]),
                    ),
                ],
            ),
        ];

        for (label, replies) in cases {
            let server = github_server(replies);
            let provider = adapter(dependencies().dependencies);
            let error = open(provider.as_ref(), &server, OpenMode::ResumeCreate)
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind, ErrorKind::PreconditionFailed, "{label}");
            assert!(server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|record| method_of(record) == "GET"),
                "{label}");
        }
    });
}

#[test]
fn a_public_repository_is_refused_and_an_occupied_root_cannot_be_created() {
    runtime().block_on(async {
        let public = github_server(vec![repository_reply(false)]);
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

        let occupied = github_server(vec![
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

        let unrelated = github_server(vec![
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

        let no_push = github_server(vec![reply(
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

        let missing = github_server(vec![
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

        let empty = github_server(vec![
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

        let present = github_server(vec![
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
        let server = github_server(vec![
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

    });
}

#[test]
fn a_lost_upload_response_converges_on_the_stored_asset_without_deleting() {
    runtime().block_on(async {
        let bytes = vec![4u8; 512];
        let server = github_server(vec![
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
        let server = github_server(vec![
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
        let server = github_server(vec![
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
        let server = github_server(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(200, releases.clone()),
            reply(
                200,
                json!([
                    asset(1, "state-a", 10, None),
                    asset(2, "pack-a", 20, None),
                    asset(3, "bundle-b", 30, None),
                    asset(5, "point-a", 50, None)
                ]),
            ),
            reply(200, json!([asset(4, "state-c", 40, None)])),
            reply(200, releases.clone()),
            reply(200, json!([asset(4, "state-c", 40, None)])),
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
        let server = github_server(vec![
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

    });
}

#[test]
fn primary_and_secondary_limits_become_rate_limited_with_a_retry_instant() {
    runtime().block_on(async {
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());

        let primary = github_server(vec![Reply::Http {
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

        let secondary = github_server(vec![Reply::Http {
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

        let forbidden = github_server(vec![Reply::Http {
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

        let expired = github_server(vec![Reply::Http {
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

        let absent = github_server(vec![Reply::Http {
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
        let server = github_server(vec![repository_reply(true), reply(200, json!([]))]);
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
        let server = github_server(vec![repository_reply(true), reply(200, json!([]))]);
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
        let server = github_server(vec![repository_reply(true), reply(200, json!([]))]);
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
        let server = github_server(vec![
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
            .reconcile_upload(&handle, &intent, Some(&resume), &cancel)
            .await
            .unwrap();
        let UploadResolution::Complete(receipt) = resolution else {
            panic!("a stored asset must resolve as complete");
        };
        assert_eq!(receipt.locator.object, "60/12");
        let missing = provider
            .reconcile_upload(&handle, &intent, Some(&resume), &cancel)
            .await
            .unwrap();
        assert!(matches!(missing, UploadResolution::RestartRequired));
    });
}

#[test]
fn cancelling_during_the_body_stops_the_read() {
    runtime().block_on(async {
        let server = github_server(vec![
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
        let server = github_server(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(201, release(31, &job_tag("job-1", 0))),
            reply(
                201,
                asset(
                    77,
                    "state-object-1",
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
        let intent = object_intent(&handle, ObjectRole::SyncState, "object-1", &bytes);
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
fn descriptor_listing_reads_only_the_descriptor_release() {
    runtime().block_on(async {
        let descriptor_tag = format!("{PREFIX}-d-0");
        let server = github_server(vec![
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

#[test]
fn deleting_an_asset_checks_its_release_tag_and_role_prefix_first() {
    runtime().block_on(async {
        let tag = job_tag("job-1", 0);
        let server = github_server(vec![
            repository_reply(true),
            reply(200, json!([])),
            // The removable asset: its release is tagged by this root.
            reply(200, release(20, &tag)),
            reply(200, json!([asset(88, "pack-object-1", 10, None)])),
            reply(204, json!(null)),
            // A release this root never tagged.
            reply(200, release(21, "someone-elses-tag-0")),
            // An asset of ours whose name carries no removable role prefix.
            reply(200, release(20, &tag)),
            reply(200, json!([asset(89, "descriptor-object-1", 10, None)])),
            // An asset that is no longer in the release.
            reply(200, release(20, &tag)),
            reply(200, json!([])),
            // A release that is gone entirely.
            reply(404, json!({})),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let cancel = Cancellation::default();
        let locator = |collection: Option<&str>, object: &str| RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: collection.map(str::to_owned),
            object: object.to_owned(),
        };

        provider
            .delete_object(&handle, &locator(Some(&tag), "20/88"), &cancel)
            .await
            .unwrap();
        for refused in [
            locator(Some("someone-elses-tag-0"), "21/90"),
            locator(Some(&tag), "20/89"),
        ] {
            assert_eq!(
                provider
                    .delete_object(&handle, &refused, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        // Both shapes of "already gone" answer the same as a removal.
        for absent in [locator(Some(&tag), "20/88"), locator(Some(&tag), "22/91")] {
            provider
                .delete_object(&handle, &absent, &cancel)
                .await
                .unwrap();
        }
        // The head does not exist on this service and no locator can name one.
        assert_eq!(
            provider.head_locator(&handle).unwrap_err().kind,
            ErrorKind::Unsupported
        );

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 11);
        assert!(head_line(&records[2]).starts_with("GET ") && head_line(&records[2]).contains("/releases/20 "));
        assert_eq!(
            method_of(&records[4]),
            "DELETE",
            "only the checked asset id is removed"
        );
        assert!(head_line(&records[4]).contains("/releases/assets/88 "));
        drop(records);
    });
}

/// Leases live in releases of their own. A job derived tag would let anyone who
/// can enumerate the repository count the devices writing to it.
#[test]
fn the_lease_collection_uses_its_own_tag_and_asset_prefix() {
    runtime().block_on(async {
        let tag = "0123456789abcdef0123456789abcdef";
        let work = lease_object_id(LeaseKind::Work, tag).unwrap();
        let lease_tag = format!("{PREFIX}-{}-0", api::LEASE_BATCH);
        assert_eq!(api::batch_key(ObjectRole::Lease, "job-1"), api::LEASE_BATCH);
        assert_eq!(api::asset_name(ObjectRole::Lease, &work), format!("lease-{work}"));

        let server = github_server(vec![
            repository_reply(true),
            reply(200, json!([])),
            reply(
                200,
                json!([release(31, &lease_tag), release(32, &job_tag("job-1", 0))]),
            ),
            reply(
                200,
                json!([
                    asset(1, &format!("lease-{work}"), 10, None),
                    asset(2, "pack-object-1", 20, None)
                ]),
            ),
        ]);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create)
            .await
            .unwrap();
        let page = provider
            .list_objects(&handle, Collection::Leases, None, 10, &Cancellation::default())
            .await
            .unwrap();
        // Only the lease release is visited, and only its `lease-` assets.
        assert_eq!(
            page.objects
                .iter()
                .map(|object| object.locator.object.as_str())
                .collect::<Vec<_>>(),
            vec!["31/1"]
        );
        assert_eq!(page.objects[0].locator.collection.as_deref(), Some(lease_tag.as_str()));
        assert_eq!(server.requests.lock().unwrap().len(), 4);
    });
}

#[test]
fn a_zero_length_owned_starter_recovers_after_a_failed_upload() {
    runtime().block_on(async {
        for reconcile in [false, true] {
            let bytes = vec![7u8; 64];
            let starter = json!({ "id": 88, "name": "pack-object-1", "size": 0, "state": "starter" });
            let mut replies = vec![repository_reply(true), reply(200, json!([])),
                reply(201, release(20, &job_tag("job-1", 0)))];
            if reconcile {
                replies.push(reply(502, json!({})));
                replies.push(reply(200, json!([release(20, &job_tag("job-1", 0))])));
            } else {
                replies.push(reply(422, json!({})));
            }
            replies.extend([
                reply(200, json!([starter.clone()])),
                reply(200, release(20, &job_tag("job-1", 0))),
                reply(200, starter), reply(204, json!(null)),
                reply(201, asset(99, "pack-object-1", 64, Some(digest_of(&bytes)))),
            ]);
            let server = github_server(replies);
            let test = dependencies();
            let provider = adapter(test.dependencies.clone());
            let handle = open(provider.as_ref(), &server, OpenMode::Create).await.unwrap();
            let directory = tempfile::tempdir().unwrap();
            let source = source(directory.path(), "pack", &bytes);
            let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
            let cancel = Cancellation::default();
            if reconcile {
                assert_eq!(provider.create_object(&handle, &intent, &source, None, &cancel).await.unwrap_err().kind, ErrorKind::Transient);
                assert!(matches!(provider.reconcile_upload(&handle, &intent, None, &cancel).await.unwrap(), UploadResolution::RestartRequired));
            }
            let receipt = provider.create_object(&handle, &intent, &source, None, &cancel).await.unwrap();
            assert_eq!(receipt.locator.object, "20/99");
            let requests = server.requests.lock().unwrap();
            let deleted: Vec<_> = requests.iter().filter(|r| method_of(r) == "DELETE").collect();
            assert_eq!(deleted.len(), 1);
            assert!(head_line(deleted[0]).contains("/releases/assets/88"));
            assert_eq!(requests.last().unwrap().body, bytes);
        }
    });
}

#[test]
fn starter_cleanup_refuses_changed_incomplete_and_foreign_metadata() {
    runtime().block_on(async {
        let bytes = vec![7u8; 64];
        let starter = json!({ "id": 88, "name": "pack-object-1", "size": 0, "state": "starter" });
        for case in 0..8 {
            let mut listed = starter.clone();
            let mut current = starter.clone();
            let mut owner = release(20, &job_tag("job-1", 0));
            match case {
                0 => { listed["size"] = json!(1); }
                1 => { listed["state"] = json!("uploaded"); }
                2 => { owner["tag_name"] = json!(job_tag("other-job", 0)); }
                3 => { owner["draft"] = json!(false); }
                4 => { current["state"] = json!("uploaded"); }
                5 => { current["name"] = json!("pack-other"); }
                6 => { current["size"] = json!(1); }
                _ => { current.as_object_mut().unwrap().remove("size"); }
            }
            let mut replies = vec![repository_reply(true), reply(200, json!([])),
                reply(201, release(20, &job_tag("job-1", 0))), reply(422, json!({})), reply(200, json!([listed]))];
            if case >= 2 { replies.push(reply(200, owner)); }
            if case >= 4 { replies.push(reply(200, current)); }
            let server = github_server(replies);
            let test = dependencies();
            let provider = adapter(test.dependencies.clone());
            let handle = open(provider.as_ref(), &server, OpenMode::Create).await.unwrap();
            let directory = tempfile::tempdir().unwrap();
            let source = source(directory.path(), "pack", &bytes);
            let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
            assert!(provider.create_object(&handle, &intent, &source, None, &Cancellation::default()).await.is_err(), "case {case}");
            assert!(server.requests.lock().unwrap().iter().all(|r| method_of(r) != "DELETE"), "case {case}");
        }
    });
}

#[test]
fn starter_recovery_stops_after_one_immediate_retry() {
    runtime().block_on(async {
        let bytes = vec![7u8; 64];
        let starter = json!({ "id": 88, "name": "pack-object-1", "size": 0, "state": "starter" });
        let mut replies = vec![repository_reply(true), reply(200, json!([])),
            reply(201, release(20, &job_tag("job-1", 0)))];
        for _ in 0..2 {
            replies.extend([reply(422, json!({})), reply(200, json!([starter.clone()])),
                reply(200, release(20, &job_tag("job-1", 0))), reply(200, starter.clone()), reply(204, json!(null))]);
        }
        let server = github_server(replies);
        let test = dependencies();
        let provider = adapter(test.dependencies.clone());
        let handle = open(provider.as_ref(), &server, OpenMode::Create).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), "pack", &bytes);
        let intent = object_intent(&handle, ObjectRole::Pack, "object-1", &bytes);
        let error = provider.create_object(&handle, &intent, &source, None, &Cancellation::default()).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transient);
        assert_eq!(server.requests.lock().unwrap().iter().filter(|r| method_of(r) == "POST").count(), 3);
    });
}

#[test]
fn shared_descriptor_and_lease_starters_are_never_removed() {
    runtime().block_on(async {
        let bytes = vec![7u8; 64];
        for role in [ObjectRole::Descriptor, ObjectRole::Lease] {
            let name = api::asset_name(role, "object-1");
            let tag = format!("{PREFIX}-{}-0", api::batch_key(role, "job-1"));
            let server = github_server(vec![repository_reply(true), reply(200, json!([])),
                reply(201, release(20, &tag)), reply(422, json!({})),
                reply(200, json!([{ "id": 88, "name": name, "size": 0, "state": "starter" }]))]);
            let test = dependencies();
            let provider = adapter(test.dependencies.clone());
            let handle = open(provider.as_ref(), &server, OpenMode::Create).await.unwrap();
            let directory = tempfile::tempdir().unwrap();
            let source = source(directory.path(), "object", &bytes);
            let intent = object_intent(&handle, role, "object-1", &bytes);
            assert!(provider.create_object(&handle, &intent, &source, None, &Cancellation::default()).await.is_err());
            assert!(server.requests.lock().unwrap().iter().all(|r| method_of(r) != "DELETE"));
        }
    });
}

#[test]
fn empty_job_release_cleanup_requires_complete_empty_inventory_and_no_active_owner() {
    runtime().block_on(async {
        for case in 0..8 {
            let tag = match case {
                3 => format!("{PREFIX}-l-0"),
                4 => format!("{PREFIX}-d-0"),
                _ => job_tag("job-1", 0),
            };
            let actual = if case == 5 { job_tag("other", 0) } else { tag.clone() };
            let mut replies = vec![repository_reply(true), reply(200, json!([])), reply(200, release(20, &actual))];
            if matches!(case, 0 | 2 | 6 | 7) {
                replies.push(reply(200, if case == 2 { json!([asset(88, "pack-kept", 64, None)]) }
                    else if case == 6 { json!({"incomplete": true}) } else { json!([]) }));
            }
            if case == 0 || case == 7 { replies.push(reply(if case == 7 { 202 } else { 204 }, json!(null))); }
            let server = github_server(replies);
            let test = dependencies();
            let provider = adapter(test.dependencies.clone());
            let handle = open(provider.as_ref(), &server, OpenMode::Create).await.unwrap();
            let locator = RemoteLocator { connection_identity: handle.connection_identity.clone(), collection: Some(tag), object: "20/88".into() };
            let protected = if case == 1 { vec!["job-1".to_owned()] } else { Vec::new() };
            let result = provider.delete_empty_container(&handle, &locator, &protected, &Cancellation::default()).await;
            assert_eq!(result.is_err(), case == 6 || case == 7, "case {case}");
            let records = server.requests.lock().unwrap();
            let deletes: Vec<_> = records.iter().filter(|r| method_of(r) == "DELETE").collect();
            assert_eq!(deletes.len(), usize::from(case == 0 || case == 7), "case {case}");
            if let Some(request) = deletes.first() { assert!(head_line(request).contains("/releases/20 ")); }
        }
    });
}
