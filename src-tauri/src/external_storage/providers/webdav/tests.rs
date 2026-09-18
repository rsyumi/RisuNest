use super::*;
use crate::external_storage::{
    fake::{loopback_dependencies, MemoryVault, TestDependencies},
    transfer::{SpoolSink, SpoolSource},
    wire_fixture::{Reply, WireServer},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};

const ROOT: &str = "백업 폴더";
const ACCOUNT: &str = "user@synthetic.invalid";
const SECRET_REF: &str = "vault-webdav";
const PASSWORD: &[u8] = b"app-password";
const NOW_MS: u64 = 1_700_000_000_000;
const RESERVED_NAME: &str = "obj #1?a%b+c;d&e";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn encoded_root() -> String {
    format!("/synthetic/{}", paths::encode_segment(ROOT))
}
fn reply(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    Reply::Http {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: body.to_vec(),
    }
}
fn collection_response(href: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
         <D:resourcetype><D:collection/></D:resourcetype></D:prop>\
         <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}
fn object_response(href: &str, length: u64, etag: Option<&str>) -> String {
    let etag = etag
        .map(|etag| format!("<D:getetag>{etag}</D:getetag>"))
        .unwrap_or_default();
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
         <D:getcontentlength>{length}</D:getcontentlength>{etag}<D:resourcetype/>\
         </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}
fn multistatus_reply(responses: &[String]) -> Reply {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <D:multistatus xmlns:D=\"DAV:\">{}</D:multistatus>",
        responses.concat()
    );
    reply(207, &[("Content-Type", "application/xml")], body.as_bytes())
}
/// A `Depth: 1` answer for the repository root, with whatever members the test
/// wants beside the root's own response.
fn root_listing(members: Vec<String>) -> Reply {
    let mut responses = vec![collection_response(&format!("{}/", encoded_root()))];
    responses.extend(members);
    multistatus_reply(&responses)
}
fn folder_listing(folder: &str, members: Vec<String>) -> Reply {
    let mut responses = vec![collection_response(&format!(
        "{}/{folder}/",
        encoded_root()
    ))];
    responses.extend(members);
    multistatus_reply(&responses)
}
fn initial_folder_listings(with_descriptor: bool) -> Vec<Reply> {
    ROLE_FOLDERS
        .iter()
        .map(|folder| {
            let members = if with_descriptor && *folder == DESCRIPTOR_FOLDER {
                vec![object_response(
                    &format!("{}/{folder}/descriptor-id", encoded_root()),
                    128,
                    Some("\"descriptor-etag\""),
                )]
            } else {
                Vec::new()
            };
            folder_listing(folder, members)
        })
        .collect()
}
fn descriptor_collection() -> String {
    collection_response(&format!("{}/descriptors/", encoded_root()))
}
/// The root of an established repository.
fn established_root() -> Reply {
    root_listing(vec![descriptor_collection()])
}

struct Harness {
    server: WireServer,
    test: TestDependencies,
    provider: Arc<dyn Provider>,
}
impl Harness {
    fn start(replies: Vec<Reply>) -> Self {
        let server = WireServer::start(replies);
        let test = loopback_dependencies(MemoryVault::with(SECRET_REF, PASSWORD), NOW_MS);
        let provider = create(test.dependencies.clone()).unwrap();
        Self {
            server,
            test,
            provider,
        }
    }
    fn config(&self) -> ConnectionConfig {
        ConnectionConfig {
            provider: PROVIDER_ID.to_owned(),
            profile: None,
            endpoint: self.server.url.as_str().to_owned(),
            account_id: ACCOUNT.to_owned(),
            location: BTreeMap::from([(ROOT_KEY.to_owned(), ROOT.to_owned())]),
            oauth_profile: None,
        }
    }
    fn secret(&self) -> SecretRef {
        SecretRef(SECRET_REF.to_owned())
    }
    async fn open(&self, mode: OpenMode) -> Result<(RepositoryHandle, Capabilities)> {
        self.provider
            .open_repository(
                &self.config(),
                &self.secret(),
                mode,
                &Cancellation::default(),
            )
            .await
    }
    async fn opened(&self) -> RepositoryHandle {
        self.open(OpenMode::Existing).await.unwrap().0
    }
    fn count(&self) -> usize {
        self.server.requests.lock().unwrap().len()
    }
    fn line(&self, index: usize) -> String {
        self.server.requests.lock().unwrap()[index]
            .headers
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }
    fn header(&self, index: usize, name: &str) -> Option<String> {
        self.server.requests.lock().unwrap()[index]
            .headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
    }
    fn body(&self, index: usize) -> Vec<u8> {
        self.server.requests.lock().unwrap()[index].body.clone()
    }
}

fn locator(repository: &RepositoryHandle, collection: Option<&str>, object: &str) -> RemoteLocator {
    RemoteLocator {
        connection_identity: repository.connection_identity.clone(),
        collection: collection.map(str::to_owned),
        object: object.to_owned(),
    }
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
        sha256: risunest_sync_wire::hash(bytes),
    }
}
fn source(directory: &tempfile::TempDir, name: &str, bytes: &[u8]) -> SpoolSource {
    let path = directory.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    SpoolSource::verified(&path, bytes.len() as u64, &risunest_sync_wire::hash(bytes)).unwrap()
}
fn sink(directory: &tempfile::TempDir, name: &str) -> SpoolSink {
    SpoolSink::create(&directory.path().join(name), 1024 * 1024).unwrap()
}
/// Neither `RepositoryHandle` nor `Settings` carries a `Debug`, so the error of
/// a failed open is read without unwrapping the success value.
fn kind_of<T>(result: Result<T>) -> ErrorKind {
    result.err().expect("expected a provider error").kind
}
fn head() -> HeadBytes {
    HeadBytes::new(b"synthetic-head-bytes".to_vec()).unwrap()
}

#[test]
fn existing_open_proves_credentials_in_one_request_and_reports_capabilities() {
    runtime().block_on(async {
        let harness = Harness::start(vec![established_root()]);
        let (repository, capabilities) = harness.open(OpenMode::Existing).await.unwrap();

        assert_eq!(repository.repository_id.len(), 64);
        assert!(repository
            .repository_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert!(repository.connection_identity.starts_with("webdav:"));
        assert!(repository.connection_identity.contains(ROOT));

        assert!(capabilities.immutable_create);
        assert!(capabilities.direct_complete_read);
        assert!(capabilities.stable_head_replace);
        assert!(capabilities.head_read_after_write);
        assert!(capabilities.head_retry_control);
        assert!(capabilities.snapshot_discovery);
        // An ETag header is never CAS evidence; only the probe can raise these.
        assert!(!capabilities.atomic_create_head);
        assert!(!capabilities.conditional_head_update);
        assert!(capabilities.conditional_get);
        assert!(!capabilities.range);
        assert!(!capabilities.resumable_upload);
        assert_eq!(capabilities.upload_alignment, 1);
        assert_eq!(capabilities.sdk_overhead_bytes, 0);
        assert_eq!(capabilities.max_stored_bytes, None);
        assert!(capabilities
            .require(PublicationStrategy::Sequential)
            .is_ok());
        assert_eq!(
            capabilities
                .require(PublicationStrategy::Cas)
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(capabilities.payload_limit(64).unwrap(), None);

        assert_eq!(harness.count(), 1);
        assert_eq!(
            harness.line(0),
            format!("PROPFIND {}/ HTTP/1.1", encoded_root())
        );
        assert_eq!(harness.header(0, "depth").as_deref(), Some("1"));
        assert_eq!(harness.body(0), PROPFIND_BODY);
        let expected = format!(
            "Basic {}",
            STANDARD.encode(format!("{ACCOUNT}:{}", String::from_utf8_lossy(PASSWORD)))
        );
        assert_eq!(harness.header(0, "authorization"), Some(expected));
    });
}

#[test]
fn koofr_profile_binds_the_documented_endpoint() {
    let mut config = ConnectionConfig {
        provider: PROVIDER_ID.to_owned(),
        profile: Some("koofr".to_owned()),
        endpoint: KOOFR_ENDPOINT.to_owned(),
        account_id: ACCOUNT.to_owned(),
        location: BTreeMap::from([(ROOT_KEY.to_owned(), ROOT.to_owned())]),
        oauth_profile: None,
    };
    let accepted = settings(&config).unwrap();
    assert_eq!(accepted.profile, Profile::Koofr);
    assert_eq!(accepted.endpoint.as_str(), KOOFR_ENDPOINT);

    config.endpoint = "https://dav.elsewhere.invalid/dav/Koofr".to_owned();
    assert_eq!(kind_of(settings(&config)), ErrorKind::Unsupported);
    config.endpoint = "https://app.koofr.net/other/Koofr".to_owned();
    assert_eq!(kind_of(settings(&config)), ErrorKind::Unsupported);

}

#[test]
fn configuration_and_secret_rejections_never_reach_the_wire() {
    runtime().block_on(async {
        let harness = Harness::start(Vec::new());
        let base = harness.config();
        let broken = [
            ConnectionConfig {
                provider: "s3".to_owned(),
                ..base.clone()
            },
            ConnectionConfig {
                profile: Some("dropbox".to_owned()),
                ..base.clone()
            },
            ConnectionConfig {
                endpoint: "http://dav.synthetic.invalid/dav".to_owned(),
                ..base.clone()
            },
            ConnectionConfig {
                endpoint: format!("{}?token=secret", base.endpoint),
                ..base.clone()
            },
            ConnectionConfig {
                endpoint: "not a url".to_owned(),
                ..base.clone()
            },
            ConnectionConfig {
                account_id: "user:name".to_owned(),
                ..base.clone()
            },
            ConnectionConfig {
                account_id: String::new(),
                ..base.clone()
            },
            ConnectionConfig {
                location: BTreeMap::new(),
                ..base.clone()
            },
            ConnectionConfig {
                location: BTreeMap::from([("folder".to_owned(), ROOT.to_owned())]),
                ..base.clone()
            },
            ConnectionConfig {
                location: BTreeMap::from([
                    (ROOT_KEY.to_owned(), ROOT.to_owned()),
                    ("extra".to_owned(), "x".to_owned()),
                ]),
                ..base.clone()
            },
            ConnectionConfig {
                location: BTreeMap::from([(ROOT_KEY.to_owned(), "repo/../escape".to_owned())]),
                ..base.clone()
            },
            ConnectionConfig {
                location: BTreeMap::from([(ROOT_KEY.to_owned(), String::new())]),
                ..base.clone()
            },
            ConnectionConfig {
                oauth_profile: Some(OAuthProfile {
                    project_id: "project".to_owned(),
                    platform_client_ids: BTreeMap::new(),
                }),
                ..base.clone()
            },
        ];
        for config in broken {
            let outcome = harness
                .provider
                .open_repository(
                    &config,
                    &harness.secret(),
                    OpenMode::Existing,
                    &Cancellation::default(),
                )
                .await;
            assert_eq!(kind_of(outcome), ErrorKind::Unsupported);
        }

        let absent = harness
            .provider
            .open_repository(
                &base,
                &SecretRef("missing".to_owned()),
                OpenMode::Existing,
                &Cancellation::default(),
            )
            .await;
        assert_eq!(kind_of(absent), ErrorKind::ReauthRequired);

        for password in [b"".to_vec(), b"line\nbreak".to_vec(), vec![0xffu8; 4]] {
            let test = loopback_dependencies(MemoryVault::with(SECRET_REF, &password), NOW_MS);
            let provider = create(test.dependencies.clone()).unwrap();
            let outcome = provider
                .open_repository(
                    &base,
                    &harness.secret(),
                    OpenMode::Existing,
                    &Cancellation::default(),
                )
                .await;
            assert_eq!(kind_of(outcome), ErrorKind::ReauthRequired);
        }
        assert_eq!(harness.count(), 0);
    });
}

#[test]
fn create_refuses_an_occupied_root_and_existing_requires_the_descriptor_collection() {
    runtime().block_on(async {
        let occupied = Harness::start(vec![root_listing(vec![object_response(
            &format!("{}/head", encoded_root()),
            12,
            Some("\"head-1\""),
        )])]);
        assert_eq!(
            kind_of(occupied.open(OpenMode::Create).await),
            ErrorKind::PreconditionFailed
        );
        assert_eq!(occupied.count(), 1);

        let taken = Harness::start(vec![root_listing(vec![descriptor_collection()])]);
        assert_eq!(
            kind_of(taken.open(OpenMode::Create).await),
            ErrorKind::PreconditionFailed
        );

        let missing = Harness::start(vec![reply(404, &[], b"")]);
        assert_eq!(
            kind_of(missing.open(OpenMode::Existing).await),
            ErrorKind::NotFound
        );
        assert_eq!(missing.count(), 1);

        let bare = Harness::start(vec![root_listing(Vec::new())]);
        assert_eq!(
            kind_of(bare.open(OpenMode::Existing).await),
            ErrorKind::NotFound
        );
    });
}

#[test]
fn create_builds_the_root_and_every_role_collection_without_touching_ancestors() {
    runtime().block_on(async {
        let mut replies = vec![reply(404, &[], b"")];
        replies.extend((0..7).map(|_| reply(201, &[], b"")));
        let harness = Harness::start(replies);
        harness.open(OpenMode::Create).await.unwrap();
        assert_eq!(harness.count(), 8);
        assert_eq!(
            harness.line(1),
            format!("MKCOL {}/ HTTP/1.1", encoded_root())
        );
        let created: Vec<String> = (2..8).map(|index| harness.line(index)).collect();
        for folder in ROLE_FOLDERS {
            assert!(
                created.contains(&format!("MKCOL {}/{folder}/ HTTP/1.1", encoded_root())),
                "{folder}"
            );
        }

        // An empty root that already exists is reused; only its contents decide.
        let mut existing = vec![root_listing(Vec::new())];
        existing.extend((0..6).map(|_| reply(405, &[], b"")));
        let reuse = Harness::start(existing);
        reuse.open(OpenMode::Create).await.unwrap();
        assert_eq!(reuse.count(), 7);
        assert!(reuse.line(1).starts_with("MKCOL "));
    });
}

#[test]
fn reads_stream_percent_encoded_paths_and_honour_a_conditional_request() {
    runtime().block_on(async {
        let payload = vec![7u8; 4096];
        let harness = Harness::start(vec![
            established_root(),
            reply(200, &[("ETag", "\"strong-1\"")], &payload),
            reply(304, &[("ETag", "\"strong-1\"")], b""),
            reply(200, &[("ETag", "W/\"weak-1\"")], &payload),
        ]);
        let repository = harness.opened().await;
        let directory = tempfile::tempdir().unwrap();
        let target = locator(
            &repository,
            Some("packs"),
            &format!("packs/{RESERVED_NAME}"),
        );

        let mut first = sink(&directory, "first");
        let receipt = harness
            .provider
            .read_object(
                &repository,
                &target,
                None,
                &mut first,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let ReadReceipt::Body(body) = receipt else {
            panic!("expected a body receipt");
        };
        assert_eq!(body.byte_length, payload.len() as u64);
        assert_eq!(body.version, Some(VersionToken("\"strong-1\"".to_owned())));
        let checksum = body.checksum.unwrap();
        assert_eq!(checksum.algorithm, "sha256");
        assert_eq!(checksum.value, risunest_sync_wire::hash(&payload));
        assert!(!checksum.provider_verified);
        assert!(body.complete);
        assert!(first.is_verified());
        assert_eq!(
            harness.line(1),
            format!(
                "GET {}/packs/{} HTTP/1.1",
                encoded_root(),
                paths::encode_segment(RESERVED_NAME)
            )
        );

        let mut second = sink(&directory, "second");
        let unchanged = harness
            .provider
            .read_object(
                &repository,
                &target,
                Some(&VersionToken("\"strong-1\"".to_owned())),
                &mut second,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            unchanged,
            ReadReceipt::NotModified(VersionToken("\"strong-1\"".to_owned()))
        );
        assert_eq!(
            harness.header(2, "if-none-match").as_deref(),
            Some("\"strong-1\"")
        );

        // A weak tag identifies nothing for a conditional write, so it yields
        // no version token at all.
        let mut third = sink(&directory, "third");
        let weak = harness
            .provider
            .read_object(
                &repository,
                &target,
                None,
                &mut third,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let ReadReceipt::Body(weak) = weak else {
            panic!("expected a body receipt");
        };
        assert_eq!(weak.version, None);
        assert_eq!(
            harness
                .provider
                .compare_exchange_head(
                    &repository,
                    &target,
                    &ExpectedHead::Exact(VersionToken("W/\"weak-1\"".to_owned())),
                    &head(),
                    &Cancellation::default(),
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(harness.count(), 4);
    });
}

#[test]
fn a_half_written_head_fails_verification_and_leaves_the_sink_untouched() {
    runtime().block_on(async {
        let harness = Harness::start(vec![established_root(), Reply::DelayedBody]);
        let repository = harness.opened().await;
        let directory = tempfile::tempdir().unwrap();
        let mut staging = sink(&directory, "head");
        let outcome = harness
            .provider
            .read_object(
                &repository,
                &locator(&repository, None, HEAD_OBJECT),
                None,
                &mut staging,
                &Cancellation::default(),
            )
            .await;
        assert!(outcome.is_err());
        assert!(!staging.is_verified());
    });
}

#[test]
fn immutable_create_converges_on_retry_and_refuses_a_different_length() {
    runtime().block_on(async {
        let bytes = vec![3u8; 512];
        let object = format!(
            "{}/packs/{}",
            encoded_root(),
            paths::encode_segment("pack-1")
        );
        let harness = Harness::start(vec![
            established_root(),
            reply(201, &[("ETag", "\"pack-v1\"")], b""),
            reply(412, &[], b""),
            multistatus_reply(&[object_response(&object, 512, Some("\"pack-v1\""))]),
            reply(412, &[], b""),
            multistatus_reply(&[object_response(&object, 511, Some("\"pack-v1\""))]),
        ]);
        let repository = harness.opened().await;
        let directory = tempfile::tempdir().unwrap();
        let payload = source(&directory, "pack", &bytes);
        let declared = intent(&repository, "pack-1", ObjectRole::Pack, &bytes);

        let receipt = harness
            .provider
            .create_object(
                &repository,
                &declared,
                &payload,
                None,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.byte_length, 512);
        assert_eq!(receipt.locator.object, "packs/pack-1");
        assert_eq!(receipt.locator.collection.as_deref(), Some("packs"));
        assert_eq!(
            receipt.version,
            Some(VersionToken("\"pack-v1\"".to_owned()))
        );
        let checksum = receipt.checksum.clone().unwrap();
        assert_eq!(checksum.value, declared.sha256);
        assert!(!checksum.provider_verified);
        assert_eq!(harness.line(1), format!("PUT {object} HTTP/1.1"));
        assert_eq!(harness.header(1, "if-none-match").as_deref(), Some("*"));
        assert_eq!(harness.body(1), bytes);

        // The second attempt loses the race, then converges on the same object.
        let retried = harness
            .provider
            .create_object(
                &repository,
                &declared,
                &payload,
                None,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(retried.locator, receipt.locator);
        assert!(retried.complete);
        assert_eq!(harness.line(3), format!("PROPFIND {object} HTTP/1.1"));
        assert_eq!(harness.header(3, "depth").as_deref(), Some("0"));

        // A stored object of another length is never replaced.
        let conflict = harness
            .provider
            .create_object(
                &repository,
                &declared,
                &payload,
                None,
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(conflict.kind, ErrorKind::PreconditionFailed);
        assert_eq!(conflict.http_status, Some(412));

        // No session exists, so nothing is resumable and the length must match.
        assert!(harness
            .provider
            .begin_upload(&repository, &declared, &Cancellation::default())
            .await
            .unwrap()
            .is_none());
        let mismatched = ObjectIntent {
            byte_length: 511,
            ..declared.clone()
        };
        assert_eq!(
            harness
                .provider
                .create_object(
                    &repository,
                    &mismatched,
                    &payload,
                    None,
                    &Cancellation::default()
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(harness.count(), 6);
    });
}

#[test]
fn a_lost_put_reconciles_from_the_stored_resource() {
    runtime().block_on(async {
        let bytes = vec![9u8; 300];
        let object = format!(
            "{}/packs/{}",
            encoded_root(),
            paths::encode_segment("pack-2")
        );
        let harness = Harness::start(vec![
            established_root(),
            Reply::Lost,
            multistatus_reply(&[object_response(&object, 120, None)]),
            reply(404, &[], b""),
            multistatus_reply(&[object_response(&object, 300, Some("\"pack-v2\""))]),
        ]);
        let repository = harness.opened().await;
        let directory = tempfile::tempdir().unwrap();
        let payload = source(&directory, "pack", &bytes);
        let declared = intent(&repository, "pack-2", ObjectRole::Pack, &bytes);
        let resume = ResumeState {
            sealed_state: harness.secret(),
            confirmed_offset: 0,
            expires_at_ms: None,
        };

        assert_eq!(
            harness
                .provider
                .create_object(
                    &repository,
                    &declared,
                    &payload,
                    None,
                    &Cancellation::default()
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Transient
        );

        // A truncated resource under the same name is a conflict, not progress.
        let partial = harness
            .provider
            .reconcile_upload(
                &repository,
                &declared,
                Some(&resume),
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert!(matches!(partial, UploadResolution::Conflict));

        let absent = harness
            .provider
            .reconcile_upload(
                &repository,
                &declared,
                Some(&resume),
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert!(matches!(absent, UploadResolution::RestartRequired));

        let complete = harness
            .provider
            .reconcile_upload(
                &repository,
                &declared,
                Some(&resume),
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let UploadResolution::Complete(receipt) = complete else {
            panic!("expected a complete resolution");
        };
        assert_eq!(receipt.byte_length, 300);
        assert_eq!(receipt.locator.object, "packs/pack-2");
        assert_eq!(
            receipt.version,
            Some(VersionToken("\"pack-v2\"".to_owned()))
        );
        assert_eq!(harness.count(), 5);
    });
}

#[test]
fn head_writes_send_one_conditional_request_and_read_back_the_same_bytes() {
    runtime().block_on(async {
        let head_path = format!("{}/head", encoded_root());
        let harness = Harness::start(vec![
            established_root(),
            reply(201, &[("ETag", "\"head-1\"")], b""),
            reply(412, &[], b""),
            reply(204, &[("ETag", "\"head-2\"")], b""),
            reply(200, &[("ETag", "\"head-2\"")], head().as_bytes()),
        ]);
        let repository = harness.opened().await;
        let target = locator(&repository, None, HEAD_OBJECT);

        let created = harness
            .provider
            .compare_exchange_head(
                &repository,
                &target,
                &ExpectedHead::Absent,
                &head(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            created,
            HeadReceipt {
                version: Some(VersionToken("\"head-1\"".to_owned())),
                complete: true,
            }
        );
        assert_eq!(harness.line(1), format!("PUT {head_path} HTTP/1.1"));
        assert_eq!(harness.header(1, "if-none-match").as_deref(), Some("*"));
        assert_eq!(harness.body(1), head().as_bytes());

        let stale = harness
            .provider
            .compare_exchange_head(
                &repository,
                &target,
                &ExpectedHead::Exact(VersionToken("\"head-0\"".to_owned())),
                &head(),
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(stale.kind, ErrorKind::PreconditionFailed);
        assert_eq!(stale.http_status, Some(412));
        assert_eq!(harness.header(2, "if-match").as_deref(), Some("\"head-0\""));
        assert_eq!(harness.header(2, "if-none-match"), None);

        let replaced = harness
            .provider
            .replace_head(&repository, &target, &head(), &Cancellation::default())
            .await
            .unwrap();
        assert_eq!(
            replaced.version,
            Some(VersionToken("\"head-2\"".to_owned()))
        );
        assert!(replaced.complete);
        assert_eq!(harness.header(3, "if-match"), None);
        assert_eq!(harness.header(3, "if-none-match"), None);

        let directory = tempfile::tempdir().unwrap();
        let mut staging = sink(&directory, "head");
        let read_back = harness
            .provider
            .read_object(
                &repository,
                &target,
                None,
                &mut staging,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let ReadReceipt::Body(body) = read_back else {
            panic!("expected a body receipt");
        };
        assert_eq!(body.byte_length, head().as_bytes().len() as u64);
        assert_eq!(
            body.checksum.unwrap().value,
            risunest_sync_wire::hash(head().as_bytes())
        );
        assert!(staging.is_verified());
        assert_eq!(harness.count(), 5);
    });
}

#[test]
fn resume_create_recovers_lost_collection_creation_without_overwriting_descriptors() {
    runtime().block_on(async {
        let present: Vec<String> = ROLE_FOLDERS[..3]
            .iter()
            .map(|folder| collection_response(&format!("{}/{folder}/", encoded_root())))
            .collect();
        let mut replies = vec![reply(404, &[], b""), Reply::Lost, root_listing(present)];
        replies.extend(ROLE_FOLDERS[3..].iter().map(|_| reply(201, &[], b"")));
        replies.extend(initial_folder_listings(true));
        let harness = Harness::start(replies);
        assert!(harness.open(OpenMode::ResumeCreate).await.is_err());
        harness.open(OpenMode::ResumeCreate).await.unwrap();
        assert_eq!(
            harness.count(),
            3 + ROLE_FOLDERS.len() - ROLE_FOLDERS[..3].len() + ROLE_FOLDERS.len()
        );
        assert_eq!(
            harness.line(1),
            format!("MKCOL {}/ HTTP/1.1", encoded_root())
        );
        let created_end = 3 + ROLE_FOLDERS.len() - ROLE_FOLDERS[..3].len();
        assert!((3..created_end).all(|index| harness.line(index).starts_with("MKCOL ")));
        assert!((0..harness.count()).all(|index| !harness.line(index).starts_with("PUT ")));
    });
}

#[test]
fn resume_create_rejects_foreign_role_contents_duplicate_descriptors_and_incomplete_lists() {
    runtime().block_on(async {
        let root_members: Vec<String> = ROLE_FOLDERS
            .iter()
            .map(|folder| collection_response(&format!("{}/{folder}/", encoded_root())))
            .collect();

        let harness = Harness::start(vec![
            root_listing(root_members.clone()),
            folder_listing(
                ROLE_FOLDERS[0],
                vec![object_response(
                    &format!("{}/{}/foreign", encoded_root(), ROLE_FOLDERS[0]),
                    4,
                    None,
                )],
            ),
        ]);
        assert_eq!(
            kind_of(harness.open(OpenMode::ResumeCreate).await),
            ErrorKind::PreconditionFailed
        );

        let payload = Harness::start(vec![root_listing(vec![collection_response(&format!(
            "{}/packs/",
            encoded_root()
        ))])]);
        assert_eq!(
            kind_of(payload.open(OpenMode::Create).await),
            ErrorKind::PreconditionFailed
        );
        assert_eq!(payload.count(), 1);
        assert_eq!(harness.count(), 2);

        let harness = Harness::start(vec![
            root_listing(root_members.clone()),
            folder_listing(ROLE_FOLDERS[0], Vec::new()),
            folder_listing(
                DESCRIPTOR_FOLDER,
                vec![
                    object_response(
                        &format!("{}/{DESCRIPTOR_FOLDER}/descriptor-a", encoded_root()),
                        64,
                        None,
                    ),
                    object_response(
                        &format!("{}/{DESCRIPTOR_FOLDER}/descriptor-b", encoded_root()),
                        64,
                        None,
                    ),
                ],
            ),
        ]);
        assert_eq!(
            kind_of(harness.open(OpenMode::ResumeCreate).await),
            ErrorKind::PreconditionFailed
        );

        let harness = Harness::start(vec![
            root_listing(root_members),
            multistatus_reply(&[object_response(
                &format!("{}/{}/foreign", encoded_root(), ROLE_FOLDERS[0]),
                4,
                None,
            )]),
        ]);
        assert_eq!(
            kind_of(harness.open(OpenMode::ResumeCreate).await),
            ErrorKind::PreconditionFailed
        );
        assert_eq!(harness.count(), 2);
    });
}

#[test]
fn resume_create_rejects_duplicates_conflicting_types_unknown_members_and_405() {
    runtime().block_on(async {
        let invalid = [
            multistatus_reply(&[object_response(&encoded_root(), 0, None)]),
            multistatus_reply(&[
                collection_response(&format!("{}/", encoded_root())),
                collection_response(&format!("{}/", encoded_root())),
            ]),
            root_listing(vec![descriptor_collection(), descriptor_collection()]),
            root_listing(vec![object_response(
                &format!("{}/descriptors", encoded_root()),
                1,
                None,
            )]),
            root_listing(vec![
                descriptor_collection(),
                collection_response(&format!("{}/foreign/", encoded_root())),
            ]),
        ];
        for listing in invalid {
            let harness = Harness::start(vec![listing]);
            assert_eq!(
                kind_of(harness.open(OpenMode::ResumeCreate).await),
                ErrorKind::PreconditionFailed
            );
            assert_eq!(harness.count(), 1);
        }

        let existing: Vec<String> = ROLE_FOLDERS[1..]
            .iter()
            .map(|folder| collection_response(&format!("{}/{folder}/", encoded_root())))
            .collect();
        let harness = Harness::start(vec![root_listing(existing), reply(405, &[], b"")]);
        assert_eq!(
            kind_of(harness.open(OpenMode::ResumeCreate).await),
            ErrorKind::PreconditionFailed
        );
        assert_eq!(harness.count(), 2);
    });
}

#[test]
fn a_lost_head_response_records_exactly_one_write() {
    runtime().block_on(async {
        for conditional in [false, true] {
            let harness = Harness::start(vec![established_root(), Reply::Lost]);
            let repository = harness.opened().await;
            let target = locator(&repository, None, HEAD_OBJECT);
            let outcome = if conditional {
                harness
                    .provider
                    .compare_exchange_head(
                        &repository,
                        &target,
                        &ExpectedHead::Absent,
                        &head(),
                        &Cancellation::default(),
                    )
                    .await
            } else {
                harness
                    .provider
                    .replace_head(&repository, &target, &head(), &Cancellation::default())
                    .await
            };
            assert_eq!(outcome.unwrap_err().kind, ErrorKind::Transient);
            assert_eq!(harness.count(), 2, "conditional: {conditional}");
        }
    });
}

#[test]
fn listings_page_by_name_with_a_cursor_and_reject_limits_outside_the_range() {
    runtime().block_on(async {
        let folder = format!("{}/snapshots", encoded_root());
        let names = ["snap-a", "snap-b", "snap-c"];
        let page = || {
            let mut responses = vec![collection_response(&format!("{folder}/"))];
            responses.extend(names.iter().enumerate().map(|(index, name)| {
                object_response(
                    &format!("{folder}/{name}"),
                    (index as u64 + 1) * 10,
                    Some("\"snap\""),
                )
            }));
            // A member that is gone and a nested collection are both skipped.
            responses.push(format!(
                "<D:response><D:href>{folder}/ghost</D:href>\
                 <D:status>HTTP/1.1 404 Not Found</D:status></D:response>"
            ));
            responses.push(collection_response(&format!("{folder}/nested/")));
            multistatus_reply(&responses)
        };
        let harness = Harness::start(vec![established_root(), page(), page()]);
        let repository = harness.opened().await;

        let first = harness
            .provider
            .list_objects(
                &repository,
                Collection::Snapshots,
                None,
                2,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(first.objects.len(), 2);
        assert_eq!(first.objects[0].locator.object, "snapshots/snap-a");
        assert_eq!(
            first.objects[0].locator.collection.as_deref(),
            Some("snapshots")
        );
        assert_eq!(first.objects[0].byte_length, 10);
        assert_eq!(first.objects[1].byte_length, 20);
        assert!(first.objects.iter().all(|object| object.complete));
        assert_eq!(first.next_cursor.as_deref(), Some("snap-b"));
        assert_eq!(harness.line(1), format!("PROPFIND {folder}/ HTTP/1.1"));
        assert_eq!(harness.header(1, "depth").as_deref(), Some("1"));

        let second = harness
            .provider
            .list_objects(
                &repository,
                Collection::Snapshots,
                first.next_cursor.as_deref(),
                2,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(second.objects.len(), 1);
        assert_eq!(second.objects[0].locator.object, "snapshots/snap-c");
        assert_eq!(second.next_cursor, None);

        for limit in [0u16, 1001] {
            assert_eq!(
                harness
                    .provider
                    .list_objects(
                        &repository,
                        Collection::BackupPoints,
                        None,
                        limit,
                        &Cancellation::default()
                    )
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        assert_eq!(harness.count(), 3);
    });
}

#[test]
fn foreign_handles_and_locators_are_refused_before_any_request() {
    runtime().block_on(async {
        let harness = Harness::start(vec![established_root()]);
        let repository = harness.opened().await;
        let directory = tempfile::tempdir().unwrap();
        let cancel = Cancellation::default();

        let foreign = crate::external_storage::fake::repository();
        let mut staging = sink(&directory, "foreign");
        assert_eq!(
            harness
                .provider
                .read_object(
                    &foreign,
                    &crate::external_storage::fake::locator(),
                    None,
                    &mut staging,
                    &cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );

        let rejected = [
            RemoteLocator {
                connection_identity: "webdav:other".to_owned(),
                collection: None,
                object: HEAD_OBJECT.to_owned(),
            },
            locator(&repository, None, "packs/../../escape"),
            locator(&repository, None, "/"),
            // The declared collection has to be the object's own first segment.
            locator(&repository, Some("packs"), "snapshots/snap-a"),
            locator(&repository, Some("packs"), "packs"),
        ];
        for (index, target) in rejected.into_iter().enumerate() {
            let mut staging = sink(&directory, &format!("sink-{index}"));
            assert_eq!(
                harness
                    .provider
                    .read_object(&repository, &target, None, &mut staging, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Corrupt,
                "{}",
                target.object
            );
        }

        // An object id that is not a single member name never becomes a path.
        let bytes = vec![1u8; 8];
        let payload = source(&directory, "pack", &bytes);
        let escaping = intent(&repository, "../head", ObjectRole::Pack, &bytes);
        assert_eq!(
            harness
                .provider
                .create_object(&repository, &escaping, &payload, None, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(harness.count(), 1);
    });
}

#[test]
fn documented_statuses_map_to_provider_errors_with_their_retry_hints() {
    runtime().block_on(async {
        let harness = Harness::start(vec![
            established_root(),
            reply(401, &[("WWW-Authenticate", "Basic realm=\"dav\"")], b""),
            reply(507, &[], b""),
            reply(423, &[], b""),
            reply(429, &[("Retry-After", "60")], b""),
            reply(404, &[], b""),
            reply(302, &[("Location", "https://elsewhere.invalid/dav")], b""),
        ]);
        let repository = harness.opened().await;
        let directory = tempfile::tempdir().unwrap();
        let target = locator(&repository, None, HEAD_OBJECT);
        let expected = [
            (ErrorKind::Unauthorized, 401u16, None),
            (ErrorKind::StorageFull, 507, None),
            // A write lock another client holds clears on its own timeout.
            (ErrorKind::Transient, 423, None),
            (ErrorKind::RateLimited, 429, Some(NOW_MS + 60_000)),
            (ErrorKind::NotFound, 404, None),
            // Redirects are never followed, so the endpoint is simply wrong.
            (ErrorKind::Unsupported, 302, None),
        ];
        for (index, (kind, status, retry_at_ms)) in expected.into_iter().enumerate() {
            let mut staging = sink(&directory, &format!("read-{index}"));
            let error = harness
                .provider
                .read_object(
                    &repository,
                    &target,
                    None,
                    &mut staging,
                    &Cancellation::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "{status}");
            assert_eq!(error.http_status, Some(status));
            assert_eq!(error.retry_at_ms, retry_at_ms, "{status}");
        }
        assert_eq!(harness.count(), 7);
    });
}

#[test]
fn cancellation_during_a_body_stops_the_read() {
    runtime().block_on(async {
        let harness = Harness::start(vec![established_root(), Reply::DelayedBody]);
        let repository = harness.opened().await;
        let directory = tempfile::tempdir().unwrap();
        let mut staging = sink(&directory, "cancelled");
        let cancel = Cancellation::default();
        let target = locator(&repository, None, HEAD_OBJECT);
        let read = harness
            .provider
            .read_object(&repository, &target, None, &mut staging, &cancel);
        let trigger = async {
            while harness.count() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel.cancel();
        };
        let (outcome, ()) = tokio::time::timeout(
            std::time::Duration::from_millis(2_000),
            futures::future::join(read, trigger),
        )
        .await
        .unwrap();
        assert_eq!(outcome.unwrap_err().kind, ErrorKind::Cancelled);
        assert!(!staging.is_verified());
    });
}

#[test]
fn the_probe_enables_conditional_writes_only_when_the_server_enforces_them() {
    runtime().block_on(async {
        let enforcing = Harness::start(vec![
            reply(201, &[("ETag", "\"probe-1\"")], b""),
            reply(412, &[], b""),
            reply(412, &[], b""),
            reply(204, &[("ETag", "\"probe-2\"")], b""),
            reply(204, &[], b""),
        ]);
        let probe = probe_conditional_writes(
            &enforcing.test.dependencies,
            &enforcing.config(),
            &enforcing.secret(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            probe,
            ConditionalWriteProbe {
                create_if_absent: true,
                exact_version_update: true,
                strong_version_token: true,
                probed_at_ms: NOW_MS,
            }
        );
        assert_eq!(enforcing.count(), 5);
        assert!(enforcing
            .line(0)
            .starts_with(&format!("PUT {}/probe-", encoded_root())));
        assert_eq!(enforcing.header(0, "if-none-match").as_deref(), Some("*"));
        assert_eq!(enforcing.header(1, "if-none-match").as_deref(), Some("*"));
        assert_eq!(
            enforcing.header(2, "if-match").as_deref(),
            Some("\"risunest-probe-stale\"")
        );
        assert_eq!(
            enforcing.header(3, "if-match").as_deref(),
            Some("\"probe-1\"")
        );
        assert!(enforcing.line(4).starts_with("DELETE "));

        // A server that answers every conditional PUT with a success is not
        // doing CAS, however many ETags it returns.
        let ignoring = Harness::start(vec![
            reply(201, &[("ETag", "\"probe-1\"")], b""),
            reply(201, &[("ETag", "\"probe-2\"")], b""),
            reply(204, &[], b""),
        ]);
        let ignored = probe_conditional_writes(
            &ignoring.test.dependencies,
            &ignoring.config(),
            &ignoring.secret(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            ignored,
            ConditionalWriteProbe {
                create_if_absent: false,
                exact_version_update: false,
                strong_version_token: true,
                probed_at_ms: NOW_MS,
            }
        );
        assert_eq!(ignoring.count(), 3);

        // Without a strong tag there is nothing to condition an update on.
        let untagged = Harness::start(vec![
            reply(201, &[("ETag", "W/\"weak\"")], b""),
            reply(412, &[], b""),
            reply(204, &[], b""),
        ]);
        let weak = probe_conditional_writes(
            &untagged.test.dependencies,
            &untagged.config(),
            &untagged.secret(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            weak,
            ConditionalWriteProbe {
                create_if_absent: true,
                exact_version_update: false,
                strong_version_token: false,
                probed_at_ms: NOW_MS,
            }
        );

        // A failed round trip still removes the throwaway object.
        let broken = Harness::start(vec![reply(507, &[], b""), reply(204, &[], b"")]);
        assert_eq!(
            probe_conditional_writes(
                &broken.test.dependencies,
                &broken.config(),
                &broken.secret(),
                &Cancellation::default(),
            )
            .await
            .unwrap_err()
            .kind,
            ErrorKind::StorageFull
        );
        assert_eq!(broken.count(), 2);
        assert!(broken.line(1).starts_with("DELETE "));
    });
}

#[test]
fn only_a_strong_entity_tag_becomes_a_version_token() {
    for accepted in ["\"v1\"", "  \"v1\"  ", "\"\""] {
        assert_eq!(
            strong_entity_tag(accepted),
            Some(VersionToken(accepted.trim().to_owned()))
        );
    }
    for rejected in ["W/\"v1\"", "w/\"v1\"", "v1", "\"v1", "v1\"", "\"a\"b\""] {
        assert_eq!(strong_entity_tag(rejected), None, "{rejected}");
    }
    assert_eq!(
        strong_etag(&BTreeMap::from([("etag".to_owned(), "\"v1\"".to_owned())])),
        Some(VersionToken("\"v1\"".to_owned()))
    );
    assert_eq!(strong_etag(&BTreeMap::new()), None);
    assert_eq!(
        entity_tag_header(&VersionToken("\"v1\"".to_owned())).unwrap(),
        "\"v1\""
    );
    assert_eq!(
        entity_tag_header(&VersionToken("*".to_owned()))
            .unwrap_err()
            .kind,
        ErrorKind::Corrupt
    );
}

#[test]
fn deleting_one_member_folds_404_leaves_202_unresolved_and_refuses_the_head() {
    runtime().block_on(async {
        let harness = Harness::start(vec![
            established_root(),
            reply(204, &[], b""),
            reply(404, &[], b""),
            reply(202, &[], b""),
        ]);
        let repository = harness.opened().await;
        let cancel = Cancellation::default();
        let target = locator(&repository, Some("packs"), "packs/pack-1");
        harness
            .provider
            .delete_object(&repository, &target, &cancel)
            .await
            .unwrap();
        // A member that is not there is already in the state the caller wanted.
        harness
            .provider
            .delete_object(&repository, &target, &cancel)
            .await
            .unwrap();
        // 202 is an accepted request, not an enacted deletion.
        let accepted = harness
            .provider
            .delete_object(&repository, &target, &cancel)
            .await
            .unwrap_err();
        assert_eq!(accepted.kind, ErrorKind::Transient);
        assert_eq!(accepted.http_status, Some(202));

        for refused in [
            harness.provider.head_locator(&repository).unwrap(),
            locator(&repository, None, "descriptors/descriptor-1"),
            locator(&repository, None, "elsewhere/pack-1"),
            locator(&repository, None, "packs/nested/pack-1"),
            locator(&repository, Some("snapshots"), "packs/pack-1"),
        ] {
            assert_eq!(
                harness
                    .provider
                    .delete_object(&repository, &refused, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }

        assert_eq!(harness.count(), 4);
        assert_eq!(
            harness.line(1),
            format!("DELETE {}/packs/pack-1 HTTP/1.1", encoded_root())
        );
    });
}

/// Leases are their own collection below the root, created with the other role
/// collections and enumerated on their own.
#[test]
fn the_lease_collection_is_its_own_member_of_the_root() {
    runtime().block_on(async {
        let tag = "0123456789abcdef0123456789abcdef";
        let name = lease_object_id(LeaseKind::Work, tag).unwrap();
        assert!(ROLE_FOLDERS.contains(&"leases"));
        let harness = Harness::start(vec![
            established_root(),
            multistatus_reply(&[
                collection_response(&format!("{}/leases/", encoded_root())),
                object_response(&format!("{}/leases/{name}", encoded_root()), 30, None),
            ]),
            reply(204, &[], b""),
        ]);
        let repository = harness.opened().await;
        let page = harness
            .provider
            .list_objects(
                &repository,
                Collection::Leases,
                None,
                10,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            page.objects
                .iter()
                .map(|object| object.locator.object.clone())
                .collect::<Vec<_>>(),
            vec![format!("leases/{name}")]
        );
        harness
            .provider
            .delete_object(&repository, &page.objects[0].locator, &Cancellation::default())
            .await
            .unwrap();
        assert_eq!(
            harness.line(2),
            format!("DELETE {}/leases/{name} HTTP/1.1", encoded_root())
        );
    });
}
