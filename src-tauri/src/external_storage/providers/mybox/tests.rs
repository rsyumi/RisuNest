use super::*;
use crate::external_storage::{
    fake::{loopback_dependencies, MemoryVault, TestDependencies},
    transfer::{SpoolSink, SpoolSource},
    wire_fixture::{Reply, WireServer},
};
use std::path::Path;

/// 2027-01-15T08:00:00Z, which is 17:00 in KST.
const NOW_MS: u64 = 1_800_000_000_000;
/// 2027-01-16T00:00:00+09:00.
const NEXT_RESET_MS: u64 = 1_800_025_200_000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const SECRET: &str = "mybox-pat";
const ROOT_ID: &str = "root-1";
const ROOT_NAME: &str = "RisuNest";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn token(expires_at_ms: u64) -> Vec<u8> {
    format!("{{\"pat\":\"synthetic-token\",\"expiresAtMs\":{expires_at_ms}}}").into_bytes()
}
fn fixture() -> TestDependencies {
    loopback_dependencies(MemoryVault::with(SECRET, &token(NOW_MS + DAY_MS)), NOW_MS)
}
fn secret() -> SecretRef {
    SecretRef(SECRET.into())
}
fn settings(api: &WireServer) -> ConnectionConfig {
    ConnectionConfig {
        provider: "mybox".into(),
        profile: Some("plan30gb".into()),
        endpoint: api.url.to_string(),
        account_id: "synthetic-account".into(),
        location: BTreeMap::from([("rootFolderName".to_owned(), ROOT_NAME.to_owned())]),
        oauth_profile: None,
    }
}
fn json(status: u16, body: &str) -> Reply {
    Reply::Http {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: body.as_bytes().to_vec(),
    }
}
fn octets(status: u16, body: Vec<u8>) -> Reply {
    Reply::Http {
        status,
        headers: Vec::new(),
        body,
    }
}
fn storage_body(max_file_bytes: u64, quota_bytes: u64, used_bytes: u64) -> String {
    format!(
        "{{\"maxFileBytes\":{max_file_bytes},\"quotaBytes\":{quota_bytes},\"usedBytes\":{used_bytes},\"trashAutoDeleteDays\":5,\"fileCounts\":{{\"total\":0,\"archive\":0,\"audio\":0,\"document\":0,\"etc\":0,\"executable\":0,\"image\":0,\"video\":0}}}}"
    )
}
fn resource(name: &str, id: &str, kind: &str, size: u64) -> String {
    format!(
        "{{\"resourceId\":\"{id}\",\"name\":\"{name}\",\"size\":{size},\"type\":\"{kind}\",\"parentId\":\"{ROOT_ID}\",\"createdAt\":\"2027-01-15T17:00:00+09:00\",\"modifiedAt\":\"2027-01-15T17:00:00+09:00\",\"accessedAt\":\"2027-01-15T17:00:00+09:00\",\"isFavorite\":false,\"isHidden\":false,\"lastModifiedBy\":\"synthetic\"}}"
    )
}
fn listing(items: &[String], next_cursor: Option<&str>) -> String {
    let meta = match next_cursor {
        Some(cursor) => format!("{{\"nextCursor\":\"{cursor}\"}}"),
        None => "{}".to_owned(),
    };
    format!(
        "{{\"fileCount\":0,\"subFolderCount\":0,\"resources\":[{}],\"responseMetaData\":{meta}}}",
        items.join(",")
    )
}
fn folder_id(folder: &str) -> String {
    format!("f-{folder}")
}
fn role_folders() -> String {
    let items: Vec<String> = config::FOLDERS
        .iter()
        .map(|folder| resource(folder, &folder_id(folder), "folder", 0))
        .collect();
    listing(&items, None)
}
/// Storage properties, the drive root, the repository root and the descriptor
/// evidence an `Existing` open needs.
fn open_replies(max_file_bytes: u64, quota_bytes: u64, used_bytes: u64) -> Vec<Reply> {
    vec![
        json(200, &storage_body(max_file_bytes, quota_bytes, used_bytes)),
        json(
            200,
            &listing(&[resource(ROOT_NAME, ROOT_ID, "folder", 0)], None),
        ),
        json(200, &role_folders()),
        json(
            200,
            &listing(&[resource("descriptor.bin", "file-d", "file", 4)], None),
        ),
    ]
}
fn upload_reply(storage: &WireServer, offset: u64) -> Reply {
    json(
        201,
        &format!(
            "{{\"offset\":{offset},\"uploadUrl\":\"{}/upload\"}}",
            storage.url
        ),
    )
}
fn download_reply(storage: &WireServer) -> Reply {
    json(
        200,
        &format!(
            "{{\"downloadUrl\":\"{}/download\",\"expiresIn\":600}}",
            storage.url
        ),
    )
}
async fn open(
    provider: &Arc<dyn Provider>,
    api: &WireServer,
    mode: OpenMode,
    cancel: &Cancellation,
) -> Result<(RepositoryHandle, Capabilities)> {
    provider
        .open_repository(&settings(api), &secret(), mode, cancel)
        .await
}
fn object_intent(object_id: &str, role: ObjectRole, bytes: &[u8]) -> ObjectIntent {
    ObjectIntent {
        repository_id: ROOT_ID.into(),
        job_id: "job-1".into(),
        object_id: object_id.into(),
        role,
        byte_length: bytes.len() as u64,
        sha256: risunest_sync_wire::hash(bytes),
    }
}
fn spool(directory: &Path, name: &str, bytes: &[u8]) -> SpoolSource {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    SpoolSource::verified(&path, bytes.len() as u64, &risunest_sync_wire::hash(bytes)).unwrap()
}
fn daily_reservations(deps: &TestDependencies) -> usize {
    deps.budget
        .reservations
        .lock()
        .unwrap()
        .iter()
        .flat_map(|(costs, _)| costs.iter())
        .filter(|cost| cost.bucket == api::DAILY_DOWNLOAD)
        .count()
}

#[test]
fn configuration_is_refused_before_any_request_is_dispatched() {
    runtime().block_on(async {
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let cancel = Cancellation::default();
        let base = ConnectionConfig {
            provider: "mybox".into(),
            profile: None,
            endpoint: "https://open-api.mybox.naver.com/v1".into(),
            account_id: "synthetic-account".into(),
            location: BTreeMap::from([("rootFolderName".to_owned(), ROOT_NAME.to_owned())]),
            oauth_profile: None,
        };
        let cases = [
            ConnectionConfig {
                provider: "s3".into(),
                ..base.clone()
            },
            ConnectionConfig {
                endpoint: "http://open-api.mybox.naver.com/v1".into(),
                ..base.clone()
            },
            ConnectionConfig {
                endpoint: "https://user:secret@open-api.mybox.naver.com/v1".into(),
                ..base.clone()
            },
            ConnectionConfig {
                location: BTreeMap::new(),
                ..base.clone()
            },
            ConnectionConfig {
                location: BTreeMap::from([("rootPath".to_owned(), "/RisuNest".to_owned())]),
                ..base.clone()
            },
            ConnectionConfig {
                profile: Some("plan40gb".into()),
                ..base.clone()
            },
            ConnectionConfig {
                account_id: String::new(),
                ..base.clone()
            },
            ConnectionConfig {
                oauth_profile: Some(OAuthProfile {
                    project_id: "project".into(),
                    platform_client_ids: BTreeMap::new(),
                }),
                ..base.clone()
            },
        ];
        for case in cases {
            assert_eq!(
                provider
                    .open_repository(&case, &secret(), OpenMode::Existing, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        // Loopback plain HTTP stays usable for the fixture.
        assert!(config::base_url("http://127.0.0.1:9/v1").is_ok());
        assert!(deps.budget.reservations.lock().unwrap().is_empty());
    });
}

#[test]
fn absent_expired_and_rejected_tokens_all_report_reauth_required() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        for vault in [
            MemoryVault::default(),
            MemoryVault::with(SECRET, &token(NOW_MS)),
            MemoryVault::with(SECRET, b"not-json"),
            MemoryVault::with(SECRET, b"{\"pat\":\"\",\"expiresAtMs\":9999999999999}"),
        ] {
            let deps = loopback_dependencies(vault, NOW_MS);
            let provider = create(deps.dependencies.clone()).unwrap();
            let api = WireServer::start(Vec::new());
            assert_eq!(
                open(&provider, &api, OpenMode::Existing, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::ReauthRequired
            );
            assert!(api.requests.lock().unwrap().is_empty());
        }
        // A token the service itself refuses is the same user action.
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(vec![json(401, "{\"code\":\"PLAT-401\"}")]);
        let failure = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(failure.kind, ErrorKind::ReauthRequired);
        assert_eq!(failure.http_status, Some(401));
        assert_eq!(failure.retry_at_ms, None);
    });
}

#[test]
fn existing_open_reports_the_account_file_limit_and_refuses_a_foreign_or_bare_root() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(open_replies(53_687_091_200, 32_212_254_720, 5_368_709_120));
        let (handle, capabilities) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        assert_eq!(handle.repository_id, ROOT_ID);
        assert_eq!(capabilities.max_stored_bytes, Some(53_687_091_200));
        assert_eq!(
            capabilities.payload_limit(1024).unwrap(),
            Some(53_687_091_200 - 1024)
        );
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
        assert_eq!(capabilities.documented_at.as_deref(), Some(DOCUMENTED_AT));
        assert_eq!(capabilities.evidence_urls.len(), EVIDENCE_URLS.len());
        assert!(!capabilities.conditional_get && !capabilities.range);
        let records = api.requests.lock().unwrap();
        assert_eq!(records.len(), 4);
        assert!(records[0]
            .headers
            .starts_with("GET /synthetic/drive/storage HTTP/1.1"));
        assert!(records[0].headers.contains("authorization: Bearer"));
        assert!(records[1]
            .headers
            .starts_with("GET /synthetic/drive/resources?"));
        assert!(records[2].headers.contains(&format!(
            "GET /synthetic/drive/folders/{ROOT_ID}/resources?"
        )));
        assert!(records[3]
            .headers
            .contains("/synthetic/drive/folders/f-descriptors/resources?"));
        drop(records);

        // Another account's drive has no folder of this name.
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(vec![
            json(200, &storage_body(1024, 1024, 0)),
            json(
                200,
                &listing(&[resource("Photos", "other", "folder", 0)], None),
            ),
        ]);
        assert_eq!(
            open(&provider, &api, OpenMode::Existing, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );
        assert_eq!(api.requests.lock().unwrap().len(), 2);

        // The folder exists but holds no descriptor.
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(vec![
            json(200, &storage_body(1024, 1024, 0)),
            json(
                200,
                &listing(&[resource(ROOT_NAME, ROOT_ID, "folder", 0)], None),
            ),
            json(200, &role_folders()),
            json(200, &listing(&[], None)),
        ]);
        assert_eq!(
            open(&provider, &api, OpenMode::Existing, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );
    });
}

#[test]
fn create_refuses_an_occupied_root_and_otherwise_provisions_the_role_folders() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(vec![
            json(200, &storage_body(1024, 1024, 0)),
            json(
                200,
                &listing(&[resource(ROOT_NAME, ROOT_ID, "folder", 0)], None),
            ),
            json(200, &role_folders()),
        ]);
        assert_eq!(
            open(&provider, &api, OpenMode::Create, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(api.requests.lock().unwrap().len(), 3);

        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let mut replies = vec![
            json(200, &storage_body(1024, 1024, 0)),
            json(200, &listing(&[], None)),
            json(
                201,
                &format!("{{\"name\":\"{ROOT_NAME}\",\"resourceId\":\"{ROOT_ID}\"}}"),
            ),
        ];
        for folder in config::FOLDERS {
            replies.push(json(
                201,
                &format!(
                    "{{\"name\":\"{folder}\",\"resourceId\":\"{}\"}}",
                    folder_id(folder)
                ),
            ));
        }
        let api = WireServer::start(replies);
        let (handle, _) = open(&provider, &api, OpenMode::Create, &cancel)
            .await
            .unwrap();
        assert_eq!(handle.repository_id, ROOT_ID);
        let records = api.requests.lock().unwrap();
        assert_eq!(records.len(), 9);
        assert!(records[2]
            .headers
            .starts_with("POST /synthetic/drive/folders HTTP/1.1"));
        assert_eq!(
            String::from_utf8(records[2].body.clone()).unwrap(),
            format!("{{\"folderName\":\"{ROOT_NAME}\"}}")
        );
        assert_eq!(
            String::from_utf8(records[3].body.clone()).unwrap(),
            format!("{{\"folderName\":\"backups\",\"parentId\":\"{ROOT_ID}\"}}")
        );
    });
}

#[test]
fn immutable_create_converges_after_a_lost_response_and_refuses_different_bytes() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let directory = tempfile::tempdir().unwrap();
        let payload = vec![7u8; 8];
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let storage = WireServer::start(vec![Reply::Lost]);
        let mut replies = open_replies(1024, 4096, 0);
        replies.extend([
            json(200, &listing(&[], None)),
            upload_reply(&storage, 0),
            // Reconcile: the service reports the whole object as received.
            upload_reply(&storage, 8),
            json(
                200,
                &listing(&[resource("pack-a.bin", "file-p", "file", 8)], None),
            ),
        ]);
        let api = WireServer::start(replies);
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        let intent = object_intent("pack-a", ObjectRole::Pack, &payload);
        let resume = provider
            .begin_upload(&handle, &intent, &cancel)
            .await
            .unwrap()
            .expect("MYBOX always issues an upload URL");
        assert_eq!(resume.confirmed_offset, 0);
        assert_eq!(
            resume.expires_at_ms,
            Some(NOW_MS + api::UPLOAD_URL_LIFETIME_MS)
        );
        let source = spool(directory.path(), "pack-a", &payload);
        assert_eq!(
            provider
                .create_object(&handle, &intent, &source, Some(&resume), &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Transient
        );
        let resolution = provider
            .reconcile_upload(&handle, &intent, &resume, &cancel)
            .await
            .unwrap();
        let receipt = match resolution {
            UploadResolution::Complete(receipt) => receipt,
            _ => panic!("a fully received object reconciles as complete"),
        };
        assert_eq!(receipt.locator.object, "packs/pack-a.bin");
        assert_eq!(receipt.locator.collection.as_deref(), Some("packs"));
        assert_eq!(receipt.byte_length, 8);
        assert!(receipt.complete);
        let checksum = receipt.checksum.unwrap();
        assert_eq!(checksum.value, intent.sha256);
        assert!(!checksum.provider_verified);

        // Same identity, different bytes: never an overwrite.
        let other = vec![9u8; 16];
        let conflicting = object_intent("pack-a", ObjectRole::Pack, &other);
        let source = spool(directory.path(), "pack-b", &other);
        assert_eq!(
            provider
                .create_object(&handle, &conflicting, &source, None, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );
        // Same identity and the same bytes converge without another transfer.
        let again = provider
            .create_object(
                &handle,
                &intent,
                &spool(directory.path(), "pack-c", &payload),
                None,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(again.locator, receipt.locator);
        assert_eq!(api.requests.lock().unwrap().len(), 8);
        let posted = storage.requests.lock().unwrap();
        assert_eq!(posted.len(), 1);
        assert!(!posted[0].headers.to_lowercase().contains("authorization"));
        assert!(posted[0]
            .headers
            .contains("content-type: multipart/form-data; boundary=risunest"));
        assert!(posted[0]
            .body
            .windows(payload.len())
            .any(|window| window == payload));
    });
}

#[test]
fn upload_size_and_free_space_are_checked_before_any_request() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(open_replies(64, 1000, 990));
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        assert_eq!(
            provider
                .begin_upload(
                    &handle,
                    &object_intent("over", ObjectRole::Pack, &vec![0; 65]),
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::FileTooLarge
        );
        assert_eq!(
            provider
                .begin_upload(
                    &handle,
                    &object_intent("full", ObjectRole::Pack, &vec![0; 20]),
                    &cancel
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::StorageFull
        );
        assert_eq!(api.requests.lock().unwrap().len(), 4);

        // Exactly at the reported limit is still accepted.
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let storage = WireServer::start(Vec::new());
        let mut replies = open_replies(64, 4096, 0);
        replies.extend([json(200, &listing(&[], None)), upload_reply(&storage, 0)]);
        let api = WireServer::start(replies);
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        assert!(provider
            .begin_upload(
                &handle,
                &object_intent("edge", ObjectRole::Pack, &vec![0; 64]),
                &cancel
            )
            .await
            .unwrap()
            .is_some());
    });
}

#[test]
fn reconcile_resumes_from_the_confirmed_offset_and_restarts_a_spent_session() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let directory = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..8u8).collect();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let storage = WireServer::start(vec![Reply::Lost, octets(201, Vec::new())]);
        let mut replies = open_replies(1024, 4096, 0);
        replies.extend([
            json(200, &listing(&[], None)),
            upload_reply(&storage, 0),
            upload_reply(&storage, 4),
            json(
                200,
                &listing(&[resource("pack-a.bin", "file-p", "file", 8)], None),
            ),
            json(404, "{\"code\":\"PLAT-404\"}"),
            json(200, &listing(&[], None)),
        ]);
        let api = WireServer::start(replies);
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        let intent = object_intent("pack-a", ObjectRole::Pack, &payload);
        let started = provider
            .begin_upload(&handle, &intent, &cancel)
            .await
            .unwrap()
            .unwrap();
        let source = spool(directory.path(), "pack-a", &payload);
        assert!(provider
            .create_object(&handle, &intent, &source, Some(&started), &cancel)
            .await
            .is_err());
        let resumed = match provider
            .reconcile_upload(&handle, &intent, &started, &cancel)
            .await
            .unwrap()
        {
            UploadResolution::Resumable(state) => state,
            _ => panic!("a partially received object resumes"),
        };
        assert_eq!(resumed.confirmed_offset, 4);
        // The session keeps one vault reference across re-issues.
        assert_eq!(resumed.sealed_state.0, started.sealed_state.0);
        let receipt = provider
            .create_object(&handle, &intent, &source, Some(&resumed), &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        let posted = storage.requests.lock().unwrap();
        assert_eq!(posted.len(), 2);
        assert!(posted[1]
            .body
            .windows(4)
            .any(|window| window == &payload[4..]));
        assert!(!posted[1].body.windows(8).any(|window| window == payload));
        drop(posted);
        let resume_request =
            String::from_utf8(api.requests.lock().unwrap()[6].body.clone()).unwrap();
        assert!(resume_request.contains("\"resume\":true"));
        assert!(resume_request.contains("\"modifiedTime\":\"2027-01-15T17:00:00+09:00\""));

        // A session the service no longer knows restarts rather than guessing.
        assert!(matches!(
            provider
                .reconcile_upload(&handle, &intent, &resumed, &cancel)
                .await
                .unwrap(),
            UploadResolution::RestartRequired
        ));
    });
}

#[test]
fn every_download_issues_a_fresh_one_time_url_and_charges_both_calls() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let directory = tempfile::tempdir().unwrap();
        let payload = vec![3u8; 12];
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let storage = WireServer::start(vec![
            // A spent or expired one-time URL is refused by the storage domain.
            octets(410, Vec::new()),
            octets(200, payload.clone()),
        ]);
        let mut replies = open_replies(1024, 4096, 0);
        replies.extend([
            json(
                200,
                &listing(&[resource("snap-a.bin", "file-s", "file", 12)], None),
            ),
            download_reply(&storage),
            download_reply(&storage),
        ]);
        let api = WireServer::start(replies);
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        let locator = config::locator(&handle.connection_identity, config::SNAPSHOTS, "snap-a.bin");
        let mut sink = SpoolSink::create(&directory.path().join("first"), 12).unwrap();
        let failure = provider
            .read_object(&handle, &locator, None, &mut sink, &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(failure.kind, ErrorKind::Transient);
        assert_eq!(failure.http_status, Some(410));
        let mut sink = SpoolSink::create(&directory.path().join("second"), 12).unwrap();
        let receipt = provider
            .read_object(&handle, &locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        match receipt {
            ReadReceipt::Body(object) => {
                assert_eq!(object.byte_length, 12);
                assert_eq!(object.version, None);
                let checksum = object.checksum.unwrap();
                assert_eq!(checksum.value, risunest_sync_wire::hash(&payload));
                assert!(!checksum.provider_verified);
            }
            ReadReceipt::NotModified(_) => panic!("MYBOX exposes no version token"),
        }
        assert!(sink.is_verified());
        // The failed attempt already spent its issuance and body call.
        assert_eq!(daily_reservations(&deps), 4);
        let records = api.requests.lock().unwrap();
        assert_eq!(records.len(), 7);
        assert!(records[5]
            .headers
            .starts_with("GET /synthetic/drive/files/file-s/download HTTP/1.1"));
        let fetched = storage.requests.lock().unwrap();
        assert_eq!(fetched.len(), 2);
        assert!(!fetched[1].headers.to_lowercase().contains("authorization"));
    });
}

#[test]
fn the_daily_download_budget_rolls_over_at_the_next_kst_midnight() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(open_replies(1024, 4096, 0));
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        let today = provider
            .request_cost(&handle, ProviderOperation::Get)
            .unwrap();
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].bucket, api::DAILY_DOWNLOAD);
        assert_eq!(today[0].units, 1);
        assert_eq!(
            today[0].reset,
            QuotaReset::At {
                unix_ms: NEXT_RESET_MS
            }
        );
        let issuance = provider
            .request_cost(&handle, ProviderOperation::DownloadUrl)
            .unwrap();
        assert_eq!(issuance.len(), 2);
        assert_eq!(issuance[0].reset, QuotaReset::Rolling { window_ms: 60_000 });
        deps.clock.set(NEXT_RESET_MS + 1);
        assert_eq!(
            provider
                .request_cost(&handle, ProviderOperation::Get)
                .unwrap()[0]
                .reset,
            QuotaReset::At {
                unix_ms: NEXT_RESET_MS + DAY_MS
            }
        );
        // An exhausted ledger stops the adapter before the wire.
        deps.budget
            .deny
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let locator = config::locator(&handle.connection_identity, config::PACKS, "pack-a.bin");
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("denied"), 16).unwrap();
        assert_eq!(
            provider
                .read_object(&handle, &locator, None, &mut sink, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::DailyQuotaExhausted
        );
        assert_eq!(api.requests.lock().unwrap().len(), 4);
    });
}

#[test]
fn collections_page_with_a_cursor_and_reject_limits_outside_the_documented_range() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let mut replies = open_replies(1024, 4096, 0);
        replies.extend([
            json(
                200,
                &listing(
                    &[
                        resource("snap-a.bin", "file-a", "file", 4),
                        resource("snap-b.bin", "file-b", "file", 5),
                        resource("nested", "file-n", "folder", 0),
                    ],
                    Some("MjA"),
                ),
            ),
            json(
                200,
                &listing(&[resource("snap-c.bin", "file-c", "file", 6)], None),
            ),
        ]);
        let api = WireServer::start(replies);
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        let first = provider
            .list_objects(&handle, Collection::Snapshots, None, 2, &cancel)
            .await
            .unwrap();
        assert_eq!(first.objects.len(), 2);
        assert_eq!(first.objects[0].locator.object, "snapshots/snap-a.bin");
        assert_eq!(first.objects[1].byte_length, 5);
        assert_eq!(first.next_cursor.as_deref(), Some("MjA"));
        let second = provider
            .list_objects(
                &handle,
                Collection::Snapshots,
                first.next_cursor.as_deref(),
                2,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(second.objects.len(), 1);
        assert_eq!(second.next_cursor, None);
        for limit in [0, 1001] {
            assert_eq!(
                provider
                    .list_objects(&handle, Collection::BackupPoints, None, limit, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        let records = api.requests.lock().unwrap();
        assert_eq!(records.len(), 6);
        assert!(records[4]
            .headers
            .contains("/synthetic/drive/folders/f-snapshots/resources?count=2"));
        assert!(records[5].headers.contains("cursor=MjA"));
    });
}

#[test]
fn head_replacement_writes_once_and_compare_exchange_stays_unsupported() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let storage = WireServer::start(vec![Reply::Lost, octets(200, Vec::new())]);
        let mut replies = open_replies(1024, 4096, 0);
        replies.extend([upload_reply(&storage, 0), upload_reply(&storage, 0)]);
        let api = WireServer::start(replies);
        let (handle, capabilities) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        assert_eq!(capabilities.atomic_create_head, Evidence::Unverified);
        assert_eq!(capabilities.conditional_head_update, Evidence::Unverified);
        assert_eq!(capabilities.stable_head_replace, Evidence::Synthetic);
        let locator = config::locator(&handle.connection_identity, config::HEADS, "sync-head.bin");
        let head = HeadBytes::new(vec![1, 2, 3, 4]).unwrap();
        assert_eq!(
            provider
                .compare_exchange_head(&handle, &locator, &ExpectedHead::Absent, &head, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(api.requests.lock().unwrap().len(), 4);
        // A lost answer is never retried inside the adapter.
        assert!(provider
            .replace_head(&handle, &locator, &head, &cancel)
            .await
            .is_err());
        assert_eq!(storage.requests.lock().unwrap().len(), 1);
        let receipt = provider
            .replace_head(&handle, &locator, &head, &cancel)
            .await
            .unwrap();
        assert_eq!(receipt.version, None);
        assert!(receipt.complete);
        let records = api.requests.lock().unwrap();
        assert_eq!(records.len(), 6);
        let issued = String::from_utf8(records[4].body.clone()).unwrap();
        assert!(issued.contains("\"isOverwrite\":true"));
        assert!(issued.contains("\"fileName\":\"sync-head.bin\""));
        assert!(issued.contains("\"parentId\":\"f-heads\""));
        drop(records);
        // Immutable roles are not head targets.
        let immutable = config::locator(&handle.connection_identity, config::PACKS, "pack-a.bin");
        assert_eq!(
            provider
                .replace_head(&handle, &immutable, &head, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
    });
}

#[test]
fn foreign_handles_locators_and_intents_are_refused_without_a_request() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let directory = tempfile::tempdir().unwrap();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let api = WireServer::start(open_replies(1024, 4096, 0));
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &cancel)
            .await
            .unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("foreign"), 16).unwrap();
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
        let cases = [
            RemoteLocator {
                connection_identity: "mybox/other".into(),
                collection: None,
                object: "packs/pack-a.bin".into(),
            },
            RemoteLocator {
                connection_identity: handle.connection_identity.clone(),
                collection: None,
                object: "unknown/pack-a.bin".into(),
            },
            RemoteLocator {
                connection_identity: handle.connection_identity.clone(),
                collection: Some("snapshots".into()),
                object: "packs/pack-a.bin".into(),
            },
            RemoteLocator {
                connection_identity: handle.connection_identity.clone(),
                collection: None,
                object: "packs/../secret".into(),
            },
        ];
        for locator in cases {
            assert_eq!(
                provider
                    .read_object(&handle, &locator, None, &mut sink, &cancel)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Corrupt
            );
        }
        let mut stranger = object_intent("pack-a", ObjectRole::Pack, &[1, 2, 3]);
        stranger.repository_id = "another-root".into();
        assert_eq!(
            provider
                .begin_upload(&handle, &stranger, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(api.requests.lock().unwrap().len(), 4);
    });
}

#[test]
fn cancellation_during_a_download_body_stops_the_transfer() {
    runtime().block_on(async {
        let open_cancel = Cancellation::default();
        let directory = tempfile::tempdir().unwrap();
        let deps = fixture();
        let provider = create(deps.dependencies.clone()).unwrap();
        let storage = WireServer::start(vec![Reply::DelayedBody]);
        let mut replies = open_replies(1024, 4096, 0);
        replies.extend([
            json(
                200,
                &listing(&[resource("snap-a.bin", "file-s", "file", 100)], None),
            ),
            download_reply(&storage),
        ]);
        let api = WireServer::start(replies);
        let (handle, _) = open(&provider, &api, OpenMode::Existing, &open_cancel)
            .await
            .unwrap();
        let locator = config::locator(&handle.connection_identity, config::SNAPSHOTS, "snap-a.bin");
        let mut sink = SpoolSink::create(&directory.path().join("partial"), 100).unwrap();
        let cancel = Cancellation::default();
        let read = async {
            assert!(provider
                .read_object(&handle, &locator, None, &mut sink, &cancel)
                .await
                .is_err());
        };
        let trigger = async {
            while storage.requests.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel.cancel();
        };
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            futures::future::join(read, trigger),
        )
        .await
        .unwrap();
        assert!(!sink.is_verified());
    });
}
