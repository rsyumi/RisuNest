//! Synthetic wire tests for the S3 adapter. Every byte here is generated, the
//! only server is the in-process loopback fixture and no account, token or
//! network outside `127.0.0.1` is involved.
use super::{
    config, create,
    profiles::{self, Addressing, Profile},
    provider::capabilities as reported_capabilities,
    sigv4,
};
use crate::external_storage::{
    auth::{SecretBytes, SecretVault},
    capabilities::Capabilities,
    contract::*,
    fake::{self, loopback_dependencies, with_transport, MemoryVault, TestDependencies},
    http::{HttpRequest, HttpResponse, HttpTransport},
    transfer::{SpoolSink, SpoolSource},
    wire_fixture::{Reply, WireRequest, WireServer},
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const ACCESS_KEY: &str = "AKIASYNTHETICEXAMPLE";
const SECRET_KEY: &str = "synthetic/secret/key/EXAMPLEKEY0123456789";
const REFERENCE: &str = "s3-access-keys";
const PREFIX: &str = "risunest";
const BUCKET: &str = "synthetic-bucket";
/// 2026-01-01T00:00:00Z, so every signature in this file is reproducible.
const NOW_MS: u64 = 1_767_225_600_000;
const STAMP: &str = "20260101T000000Z";
const EMPTY_HASH: &str = sigv4::EMPTY_PAYLOAD_SHA256;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn payload() -> Vec<u8> {
    format!(r#"{{"accessKeyId":"{ACCESS_KEY}","secretAccessKey":"{SECRET_KEY}"}}"#).into_bytes()
}

fn vault() -> MemoryVault {
    MemoryVault::with(REFERENCE, &payload())
}

fn dependencies() -> TestDependencies {
    loopback_dependencies(vault(), NOW_MS)
}

fn secret() -> SecretRef {
    SecretRef(REFERENCE.into())
}

fn config(profile: &str, endpoint: &str) -> ConnectionConfig {
    let mut location = BTreeMap::new();
    location.insert("bucket".to_owned(), BUCKET.to_owned());
    location.insert("prefix".to_owned(), PREFIX.to_owned());
    location.insert("region".to_owned(), "us-east-1".to_owned());
    location.insert("addressing".to_owned(), "path".to_owned());
    ConnectionConfig {
        provider: "s3".into(),
        profile: Some(profile.into()),
        endpoint: endpoint.into(),
        account_id: ACCESS_KEY.into(),
        location,
        oauth_profile: None,
    }
}

fn reply(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> Reply {
    Reply::Http {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body,
    }
}

/// A `HEAD` reply cannot set `Content-Length` twice, so the fixture derives it
/// from a body the client never reads.
fn metadata(length: usize, etag: &str) -> Reply {
    reply(200, &[("ETag", etag)], vec![0; length])
}

fn listing(folder: &str, names: &[&str], next: Option<&str>) -> Vec<u8> {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult \
         xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>synthetic-bucket</Name>",
    );
    let root = if folder.is_empty() {
        PREFIX.to_owned()
    } else {
        format!("{PREFIX}/{folder}")
    };
    body.push_str(&format!("<Prefix>{root}/</Prefix>"));
    body.push_str(&format!("<IsTruncated>{}</IsTruncated>", next.is_some()));
    for name in names {
        body.push_str(&format!(
            "<Contents><Key>{root}/{name}</Key><LastModified>2026-01-01T00:00:00.000Z\
             </LastModified><ETag>&quot;{name}-tag&quot;</ETag><Size>64</Size>\
             <StorageClass>STANDARD</StorageClass></Contents>"
        ));
    }
    if let Some(token) = next {
        body.push_str(&format!(
            "<NextContinuationToken>{token}</NextContinuationToken>"
        ));
    }
    body.push_str("</ListBucketResult>");
    body.into_bytes()
}

fn one_descriptor() -> Reply {
    reply(200, &[], listing("descriptors", &["descriptor-1"], None))
}

fn line(record: &WireRequest) -> String {
    record.headers.lines().next().unwrap_or_default().to_owned()
}

fn header(record: &WireRequest, name: &str) -> Option<String> {
    record
        .headers
        .lines()
        .skip(1)
        .filter_map(|entry| entry.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
}

async fn open(
    test: &TestDependencies,
    profile: &str,
    endpoint: &str,
    mode: OpenMode,
) -> Result<(Arc<dyn Provider>, RepositoryHandle, Capabilities)> {
    let provider = create(test.dependencies.clone())?;
    let (handle, capabilities) = provider
        .open_repository(
            &config(profile, endpoint),
            &secret(),
            mode,
            &Cancellation::default(),
        )
        .await?;
    Ok((provider, handle, capabilities))
}

async fn open_fails(
    test: &TestDependencies,
    profile: &str,
    endpoint: &str,
    mode: OpenMode,
) -> ProviderError {
    match open(test, profile, endpoint, mode).await {
        Ok(_) => panic!("the connection should have been refused"),
        Err(error) => error,
    }
}

async fn opened(
    test: &TestDependencies,
    profile: &str,
    server: &WireServer,
) -> (Arc<dyn Provider>, RepositoryHandle) {
    let (provider, handle, _) = open(test, profile, server.url.as_str(), OpenMode::Existing)
        .await
        .unwrap();
    (provider, handle)
}

struct Spool {
    directory: tempfile::TempDir,
    bytes: Vec<u8>,
    sha256: String,
}
impl Spool {
    fn of(length: usize) -> Self {
        let bytes: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source"), &bytes).unwrap();
        let sha256 = risunest_sync_wire::hash(&bytes);
        Self {
            directory,
            bytes,
            sha256,
        }
    }
    fn source(&self) -> SpoolSource {
        SpoolSource::verified(
            &self.directory.path().join("source"),
            self.bytes.len() as u64,
            &self.sha256,
        )
        .unwrap()
    }
    fn sink(&self, name: &str, max: u64) -> SpoolSink {
        SpoolSink::create(&self.directory.path().join(name), max).unwrap()
    }
    fn intent(&self, handle: &RepositoryHandle, object: &str, role: ObjectRole) -> ObjectIntent {
        ObjectIntent {
            repository_id: handle.repository_id.clone(),
            job_id: "synthetic-job".into(),
            object_id: object.into(),
            role,
            byte_length: self.bytes.len() as u64,
            sha256: self.sha256.clone(),
        }
    }
}

async fn seal_session(
    test: &TestDependencies,
    upload_id: &str,
    key: &str,
    part_size: u64,
) -> ResumeState {
    let document =
        format!(r#"{{"uploadId":"{upload_id}","key":"{key}","partSize":{part_size},"parts":[]}}"#);
    let sealed_state = test
        .vault
        .store(&SecretBytes(zeroize::Zeroizing::new(document.into_bytes())))
        .await
        .unwrap();
    ResumeState {
        sealed_state,
        confirmed_offset: 0,
        expires_at_ms: None,
    }
}

#[test]
fn signature_matches_independently_computed_known_answers() {
    let credentials = sigv4::Credentials {
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: zeroize::Zeroizing::new(SECRET_KEY.into()),
    };
    assert_eq!(sigv4::amz_date(NOW_MS).unwrap(), STAMP);
    assert_eq!(
        sigv4::uri_encode("a b/c~d.e_f-g+h", false),
        "a%20b/c~d.e_f-g%2Bh"
    );
    assert_eq!(sigv4::uri_encode("a/b", true), "a%2Fb");

    let mut headers = BTreeMap::new();
    headers.insert("x-amz-content-sha256".to_owned(), EMPTY_HASH.to_owned());
    headers.insert("x-amz-date".to_owned(), STAMP.to_owned());
    let read = sigv4::sign(
        &credentials,
        "us-east-1",
        "GET",
        &url::Url::parse(
            "https://synthetic-bucket.s3.synthetic.invalid/risunest/snapshots/snapshot-1",
        )
        .unwrap(),
        &headers,
        EMPTY_HASH,
        STAMP,
    )
    .unwrap();
    assert_eq!(
        read.canonical_request,
        format!(
            "GET\n/risunest/snapshots/snapshot-1\n\nhost:synthetic-bucket.s3.synthetic.invalid\n\
             x-amz-content-sha256:{EMPTY_HASH}\nx-amz-date:{STAMP}\n\n\
             host;x-amz-content-sha256;x-amz-date\n{EMPTY_HASH}"
        )
    );
    assert_eq!(
        read.string_to_sign,
        format!(
            "AWS4-HMAC-SHA256\n{STAMP}\n20260101/us-east-1/s3/aws4_request\n\
             543fd235ca7471069739bf7dce36b807f643fb3276f801cc373f87c14d1d11dd"
        )
    );
    assert_eq!(
        read.authorization,
        "AWS4-HMAC-SHA256 Credential=AKIASYNTHETICEXAMPLE/20260101/us-east-1/s3/aws4_request, \
         SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
         Signature=20e42741ab6f815550b989417657d1c8b5f0493fb50ea971cc6f78a31c89c812"
    );

    let mut headers = BTreeMap::new();
    headers.insert("if-none-match".to_owned(), "*".to_owned());
    headers.insert(
        "x-amz-content-sha256".to_owned(),
        sigv4::UNSIGNED_PAYLOAD.to_owned(),
    );
    headers.insert("x-amz-date".to_owned(), STAMP.to_owned());
    let part = sigv4::sign(
        &credentials,
        "us-east-1",
        "PUT",
        &url::Url::parse(
            "https://s3.hf.co/synthetic-namespace/synthetic-bucket/risunest/packs/pack-1\
             ?partNumber=2&uploadId=synthetic%2Fupload%2Bid",
        )
        .unwrap(),
        &headers,
        sigv4::UNSIGNED_PAYLOAD,
        STAMP,
    )
    .unwrap();
    assert_eq!(
        part.canonical_request,
        "PUT\n/synthetic-namespace/synthetic-bucket/risunest/packs/pack-1\n\
         partNumber=2&uploadId=synthetic%2Fupload%2Bid\nhost:s3.hf.co\nif-none-match:*\n\
         x-amz-content-sha256:UNSIGNED-PAYLOAD\nx-amz-date:20260101T000000Z\n\n\
         host;if-none-match;x-amz-content-sha256;x-amz-date\nUNSIGNED-PAYLOAD"
    );
    assert_eq!(
        part.authorization,
        "AWS4-HMAC-SHA256 Credential=AKIASYNTHETICEXAMPLE/20260101/us-east-1/s3/aws4_request, \
         SignedHeaders=host;if-none-match;x-amz-content-sha256;x-amz-date, \
         Signature=2aae03d3facacffbf29c81e83a148ac879888b737a6cbc6ba9d5d0aa3554952f"
    );
}

#[test]
fn configuration_validation_refuses_every_unusable_connection() {
    let endpoint = "https://synthetic-account.r2.cloudflarestorage.com";
    let refuse = |mutate: &dyn Fn(&mut ConnectionConfig)| {
        let mut config = config("r2", endpoint);
        mutate(&mut config);
        assert_eq!(
            config::validate(&config, &secret())
                .map(|_| ())
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
    };
    assert!(config::validate(&config("r2", endpoint), &secret()).is_ok());
    refuse(&|config| config.provider = "webdav".into());
    refuse(&|config| config.profile = None);
    refuse(&|config| config.profile = Some("minio".into()));
    refuse(&|config| {
        config.oauth_profile = Some(OAuthProfile {
            project_id: "synthetic".into(),
            platform_client_ids: BTreeMap::new(),
        })
    });
    refuse(&|config| config.account_id = String::new());
    refuse(&|config| config.endpoint = "http://synthetic.invalid".into());
    refuse(&|config| config.endpoint = "https://user:pass@synthetic.invalid".into());
    refuse(&|config| config.endpoint = "https://synthetic.invalid/root?query=1".into());
    refuse(&|config| config.endpoint = "ftp://synthetic.invalid".into());
    refuse(&|config| {
        config.location.insert("acl".into(), "private".into());
    });
    refuse(&|config| {
        config.location.remove("bucket");
    });
    refuse(&|config| {
        config.location.insert("bucket".into(), "Synthetic".into());
    });
    refuse(&|config| {
        config.location.remove("region");
    });
    refuse(&|config| {
        config.location.insert("prefix".into(), "../escape".into());
    });
    refuse(&|config| {
        config.location.insert("prefix".into(), "a//b".into());
    });
    refuse(&|config| {
        config.location.insert("addressing".into(), "dns".into());
    });
    // Loopback plain text is the wire fixture and nothing else.
    let mut loopback = config("r2", "http://127.0.0.1:9/synthetic");
    assert!(config::validate(&loopback, &secret()).is_ok());
    loopback.endpoint = "http://127.0.0.2:9/synthetic".into();
    assert_eq!(
        config::validate(&loopback, &secret())
            .map(|_| ())
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );

    // Hugging Face serves one namespace path and path addressing only.
    let mut hugging = config("hf", "https://s3.hf.co/synthetic-namespace");
    assert!(config::validate(&hugging, &secret()).is_ok());
    hugging
        .location
        .insert("addressing".into(), "virtual".into());
    assert!(config::validate(&hugging, &secret()).is_err());
    hugging.location.insert("addressing".into(), "path".into());
    hugging.location.insert("region".into(), "eu-west-1".into());
    assert!(config::validate(&hugging, &secret()).is_err());
    hugging.location.remove("region");
    assert!(config::validate(&hugging, &secret()).is_ok());
    hugging.endpoint = "https://s3.hf.co".into();
    assert!(config::validate(&hugging, &secret()).is_err());
}

#[test]
fn addressing_and_endpoint_paths_build_the_documented_urls() {
    let context = |profile: &str, endpoint: &str, addressing: Option<&str>| {
        let mut config = config(profile, endpoint);
        match addressing {
            Some(value) => config
                .location
                .insert("addressing".into(), value.to_owned()),
            None => config.location.remove("addressing"),
        };
        config::validate(&config, &secret()).unwrap()
    };
    let r2 = context(
        "r2",
        "https://synthetic-account.r2.cloudflarestorage.com",
        None,
    );
    assert_eq!(r2.addressing, Addressing::Virtual);
    assert_eq!(
        r2.url(config::Target::Object("risunest/head"), &[])
            .unwrap()
            .as_str(),
        "https://synthetic-bucket.synthetic-account.r2.cloudflarestorage.com/risunest/head"
    );
    let aws = context("aws", "https://s3.us-east-1.amazonaws.com", None);
    assert_eq!(aws.addressing, Addressing::Virtual);
    assert_eq!(
        aws.url(config::Target::Object("risunest/head"), &[])
            .unwrap()
            .as_str(),
        "https://synthetic-bucket.s3.us-east-1.amazonaws.com/risunest/head"
    );
    let b2 = context("b2", "https://s3.us-west-004.backblazeb2.com", None);
    assert_eq!(
        b2.url(config::Target::Object("risunest/packs/pack-1"), &[])
            .unwrap()
            .as_str(),
        "https://synthetic-bucket.s3.us-west-004.backblazeb2.com/risunest/packs/pack-1"
    );
    let hugging = context("hf", "https://s3.hf.co/synthetic-namespace", None);
    assert_eq!(hugging.addressing, Addressing::Path);
    assert_eq!(
        hugging
            .url(config::Target::Object("risunest/head"), &[])
            .unwrap()
            .as_str(),
        "https://s3.hf.co/synthetic-namespace/synthetic-bucket/risunest/head"
    );
    assert_eq!(
        hugging
            .url(
                config::Target::Bucket,
                &[
                    ("prefix".to_owned(), "risunest/snapshots/".to_owned()),
                    ("list-type".to_owned(), "2".to_owned()),
                ],
            )
            .unwrap()
            .as_str(),
        "https://s3.hf.co/synthetic-namespace/synthetic-bucket\
         ?list-type=2&prefix=risunest%2Fsnapshots%2F"
    );
    assert_eq!(
        hugging.connection_identity,
        "s3/hf/https://s3.hf.co/synthetic-namespace/synthetic-bucket/risunest"
    );
    assert_eq!(
        hugging.key("packs/pack-1").unwrap(),
        "risunest/packs/pack-1"
    );
    assert!(hugging.key("packs/../head").is_err());
    assert!(hugging.head_key("packs/pack-1").is_err());
    assert_eq!(hugging.head_key("head").unwrap(), "risunest/head");
    assert_eq!(
        hugging.head_key("heads/device-a").unwrap(),
        "risunest/heads/device-a"
    );
}

#[test]
fn absent_or_malformed_credentials_require_reauthentication_before_any_request() {
    runtime().block_on(async {
        for stored in [
            None,
            Some(br#"{"accessKeyId":"only-one"}"#.to_vec()),
            Some(br#"{"accessKeyId":"a","secretAccessKey":"b","region":"c"}"#.to_vec()),
            Some(br#"{"accessKeyId":"","secretAccessKey":"b"}"#.to_vec()),
            Some(b"not-json".to_vec()),
        ] {
            let test = match stored {
                Some(bytes) => loopback_dependencies(MemoryVault::with(REFERENCE, &bytes), NOW_MS),
                None => loopback_dependencies(MemoryVault::default(), NOW_MS),
            };
            let server = WireServer::start(vec![one_descriptor()]);
            let error = open_fails(&test, "r2", server.url.as_str(), OpenMode::Existing).await;
            assert_eq!(error.kind, ErrorKind::ReauthRequired);
            assert!(server.requests.lock().unwrap().is_empty());
        }
    });
}

#[test]
fn create_refuses_an_occupied_root_and_existing_requires_a_descriptor() {
    runtime().block_on(async {
        let empty = || reply(200, &[], listing("", &[], None));

        let test = dependencies();
        let server = WireServer::start(vec![empty()]);
        let (_, handle, reported) = open(&test, "r2", server.url.as_str(), OpenMode::Create)
            .await
            .unwrap();
        assert_eq!(
            handle.connection_identity,
            format!("s3/r2/{}/{BUCKET}/{PREFIX}", server.url.as_str())
        );
        assert_eq!(handle.repository_id, handle.connection_identity);
        assert!(reported.require(PublicationStrategy::Cas).is_ok());
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            line(&records[0]),
            "GET /synthetic/synthetic-bucket\
             ?list-type=2&max-keys=3&prefix=risunest%2F HTTP/1.1"
        );
        assert_eq!(
            header(&records[0], "x-amz-content-sha256").unwrap(),
            EMPTY_HASH
        );
        assert_eq!(header(&records[0], "x-amz-date").unwrap(), STAMP);
        assert!(header(&records[0], "authorization").unwrap().starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIASYNTHETICEXAMPLE/20260101/us-east-1/s3/aws4_request,"
        ));
        drop(records);

        let server = WireServer::start(vec![reply(
            200,
            &[],
            listing("", &["descriptors/descriptor-1"], None),
        )]);
        assert_eq!(
            open_fails(&dependencies(), "r2", server.url.as_str(), OpenMode::Create)
                .await
                .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let server = WireServer::start(vec![reply(200, &[], listing("", &["head"], None))]);
        assert_eq!(
            open_fails(&dependencies(), "r2", server.url.as_str(), OpenMode::Create)
                .await
                .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let server = WireServer::start(vec![reply(
            200,
            &[],
            listing("", &["packs/foreign-pack"], None),
        )]);
        assert_eq!(
            open_fails(&dependencies(), "r2", server.url.as_str(), OpenMode::Create)
                .await
                .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let server = WireServer::start(vec![empty()]);
        assert_eq!(
            open_fails(
                &dependencies(),
                "r2",
                server.url.as_str(),
                OpenMode::Existing
            )
            .await
            .kind,
            ErrorKind::NotFound
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let server = WireServer::start(vec![one_descriptor()]);
        assert!(open(
            &dependencies(),
            "r2",
            server.url.as_str(),
            OpenMode::Existing
        )
        .await
        .is_ok());
    });
}

fn root_listing(objects: &[(&str, u64)], next: Option<&str>) -> Vec<u8> {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult \
         xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>synthetic-bucket</Name>",
    );
    body.push_str(&format!("<Prefix>{PREFIX}/</Prefix>"));
    body.push_str(&format!("<IsTruncated>{}</IsTruncated>", next.is_some()));
    for (key, size) in objects {
        body.push_str(&format!(
            "<Contents><Key>{PREFIX}/{key}</Key><LastModified>2026-01-01T00:00:00.000Z\
             </LastModified><ETag>&quot;synthetic-tag&quot;</ETag><Size>{size}</Size>\
             <StorageClass>STANDARD</StorageClass></Contents>"
        ));
    }
    if let Some(token) = next {
        body.push_str(&format!(
            "<NextContinuationToken>{token}</NextContinuationToken>"
        ));
    }
    body.push_str("</ListBucketResult>");
    body.into_bytes()
}

#[test]
fn resume_create_accepts_only_an_empty_or_bootstrap_descriptor_control_layout() {
    runtime().block_on(async {
        let empty = || reply(200, &[], root_listing(&[], None));

        let server = WireServer::start(vec![empty()]);
        open(
            &dependencies(),
            "r2",
            server.url.as_str(),
            OpenMode::ResumeCreate,
        )
        .await
        .unwrap();
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let server = WireServer::start(vec![reply(
            200,
            &[],
            root_listing(&[("descriptors/descriptor-1", 64)], None),
        )]);
        open(
            &dependencies(),
            "r2",
            server.url.as_str(),
            OpenMode::ResumeCreate,
        )
        .await
        .unwrap();
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert!(line(&records[0]).contains("max-keys=3"));
        drop(records);

        for key in ["head", "packs/pack-1", "foreign/object"] {
            let server = WireServer::start(vec![reply(
                200,
                &[],
                root_listing(&[(key, 48)], None),
            )]);
            assert_eq!(
                open_fails(
                    &dependencies(),
                    "r2",
                    server.url.as_str(),
                    OpenMode::ResumeCreate,
                )
                .await
                .kind,
                ErrorKind::PreconditionFailed,
                "{key}"
            );
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }

        let server = WireServer::start(vec![reply(
            200,
            &[],
            root_listing(
                &[
                    ("descriptors/descriptor-1", 64),
                    ("descriptors/descriptor-2", 64),
                    ("descriptors/descriptor-3", 64),
                ],
                None,
            ),
        )]);
        assert_eq!(
            open_fails(
                &dependencies(),
                "r2",
                server.url.as_str(),
                OpenMode::ResumeCreate,
            )
            .await
            .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let zero = root_listing(&[("descriptors/descriptor-1", 0)], None);
        let server = WireServer::start(vec![reply(200, &[], zero)]);
        assert_eq!(
            open_fails(
                &dependencies(),
                "r2",
                server.url.as_str(),
                OpenMode::ResumeCreate,
            )
            .await
            .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    });
}

#[test]
fn object_create_and_read_round_trip_through_the_permanent_key_locator() {
    runtime().block_on(async {
        let spool = Spool::of(2048);
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(200, &[("ETag", "\"pack-etag\"")], Vec::new()),
            reply(200, &[("ETag", "\"pack-etag\"")], spool.bytes.clone()),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let cancel = Cancellation::default();
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        assert!(provider
            .begin_upload(&handle, &intent, &cancel)
            .await
            .unwrap()
            .is_none());
        let receipt = provider
            .create_object(&handle, &intent, &spool.source(), None, &cancel)
            .await
            .unwrap();
        assert_eq!(receipt.locator.object, "packs/pack-1");
        assert_eq!(receipt.locator.collection.as_deref(), Some("packs"));
        assert_eq!(receipt.byte_length, 2048);
        assert!(receipt.complete);
        assert_eq!(receipt.version, Some(VersionToken("\"pack-etag\"".into())));
        let checksum = receipt.checksum.clone().unwrap();
        assert_eq!(checksum.algorithm, "sha256");
        assert_eq!(checksum.value, spool.sha256);
        assert!(!checksum.provider_verified);

        let mut sink = spool.sink("received", 4096);
        let read = provider
            .read_object(&handle, &receipt.locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        match read {
            ReadReceipt::Body(body) => {
                assert_eq!(body.byte_length, 2048);
                assert_eq!(body.locator, receipt.locator);
            }
            ReadReceipt::NotModified(_) => panic!("unexpected conditional answer"),
        }
        assert!(sink.is_verified());
        assert_eq!(
            std::fs::read(spool.directory.path().join("received")).unwrap(),
            spool.bytes
        );

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            line(&records[1]),
            "PUT /synthetic/synthetic-bucket/risunest/packs/pack-1 HTTP/1.1"
        );
        assert_eq!(header(&records[1], "if-none-match").unwrap(), "*");
        assert_eq!(
            header(&records[1], "x-amz-content-sha256").unwrap(),
            spool.sha256
        );
        assert!(header(&records[1], "x-amz-checksum-sha256").is_none());
        assert_eq!(records[1].body, spool.bytes);
        assert_eq!(
            line(&records[2]),
            "GET /synthetic/synthetic-bucket/risunest/packs/pack-1 HTTP/1.1"
        );
        assert!(header(&records[2], "if-none-match").is_none());
    });
}

#[test]
fn immutable_create_converges_on_a_retry_and_refuses_different_bytes() {
    runtime().block_on(async {
        let spool = Spool::of(2048);
        let cancel = Cancellation::default();

        // A lost response is reported, never covered by a retry inside here.
        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor(), Reply::Lost]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        assert_eq!(
            provider
                .create_object(&handle, &intent, &spool.source(), None, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Transient
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);

        // The same identity and bytes converge on one complete receipt.
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(
                412,
                &[],
                b"<Error><Code>PreconditionFailed</Code></Error>".to_vec(),
            ),
            metadata(2048, "\"pack-etag\""),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        let receipt = provider
            .create_object(&handle, &intent, &spool.source(), None, &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.version, Some(VersionToken("\"pack-etag\"".into())));
        assert_eq!(server.requests.lock().unwrap().len(), 3);

        // A different length under the same name is a conflict, never a write.
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(412, &[], Vec::new()),
            metadata(4096, "\"other\""),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        assert_eq!(
            provider
                .create_object(&handle, &intent, &spool.source(), None, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::PreconditionFailed
        );

        // A source that disagrees with the intent never reaches the wire.
        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor()]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let mut wrong = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        wrong.byte_length = 2047;
        assert_eq!(
            provider
                .create_object(&handle, &wrong, &spool.source(), None, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    });
}

#[test]
fn without_a_conditional_put_the_adapter_looks_before_it_writes() {
    runtime().block_on(async {
        let spool = Spool::of(2048);
        let cancel = Cancellation::default();

        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(404, &[], Vec::new()),
            reply(200, &[("ETag", "\"b2-etag\"")], Vec::new()),
        ]);
        let (provider, handle) = opened(&test, "b2", &server).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        let receipt = provider
            .create_object(&handle, &intent, &spool.source(), None, &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            line(&records[1]),
            "HEAD /synthetic/synthetic-bucket/risunest/packs/pack-1 HTTP/1.1"
        );
        assert!(header(&records[2], "if-none-match").is_none());
        drop(records);

        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor(), metadata(2048, "\"b2-etag\"")]);
        let (provider, handle) = opened(&test, "b2", &server).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        assert!(
            provider
                .create_object(&handle, &intent, &spool.source(), None, &cancel)
                .await
                .unwrap()
                .complete
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);

        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor(), metadata(7, "\"b2-etag\"")]);
        let (provider, handle) = opened(&test, "b2", &server).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        assert_eq!(
            provider
                .create_object(&handle, &intent, &spool.source(), None, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    });
}

#[test]
fn a_checksum_receipt_claims_verification_only_when_the_service_confirms_it() {
    runtime().block_on(async {
        let spool = Spool::of(1024);
        let expected = base64_of(&spool.sha256);
        let cancel = Cancellation::default();
        for (echo, verified) in [(true, true), (false, false)] {
            let test = dependencies();
            let headers: Vec<(&str, &str)> = if echo {
                vec![("ETag", "\"g\""), ("x-amz-checksum-sha256", &expected)]
            } else {
                vec![("ETag", "\"g\"")]
            };
            let server =
                WireServer::start(vec![one_descriptor(), reply(200, &headers, Vec::new())]);
            let (provider, handle) = opened(&test, "generic", &server).await;
            let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
            let receipt = provider
                .create_object(&handle, &intent, &spool.source(), None, &cancel)
                .await
                .unwrap();
            assert_eq!(receipt.checksum.unwrap().provider_verified, verified);
            let records = server.requests.lock().unwrap();
            assert_eq!(
                header(&records[1], "x-amz-checksum-sha256").unwrap(),
                expected
            );
        }
    });
}

fn base64_of(lower_hex: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(hex::decode(lower_hex).unwrap())
}

#[test]
fn head_writes_send_exactly_one_request_and_map_a_failed_precondition() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let head = HeadBytes::new(b"synthetic-head-bytes".to_vec()).unwrap();

        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(200, &[("ETag", "\"head-1\"")], Vec::new()),
            reply(200, &[("ETag", "\"head-2\"")], Vec::new()),
            reply(
                412,
                &[],
                b"<Error><Code>PreconditionFailed</Code></Error>".to_vec(),
            ),
            Reply::Lost,
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let locator = provider.head_locator(&handle).unwrap();
        let first = provider
            .compare_exchange_head(&handle, &locator, &ExpectedHead::Absent, &head, &cancel)
            .await
            .unwrap();
        assert_eq!(
            first,
            HeadReceipt {
                version: Some(VersionToken("\"head-1\"".into())),
                complete: true
            }
        );
        let second = provider
            .compare_exchange_head(
                &handle,
                &locator,
                &ExpectedHead::Exact(VersionToken("\"head-1\"".into())),
                &head,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(second.version, Some(VersionToken("\"head-2\"".into())));
        let stale = provider
            .compare_exchange_head(
                &handle,
                &locator,
                &ExpectedHead::Exact(VersionToken("\"head-1\"".into())),
                &head,
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(stale.kind, ErrorKind::PreconditionFailed);
        assert_eq!(stale.http_status, Some(412));
        assert_eq!(
            provider
                .replace_head(&handle, &locator, &head, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Transient
        );
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(
            line(&records[1]),
            "PUT /synthetic/synthetic-bucket/risunest/head HTTP/1.1"
        );
        assert_eq!(header(&records[1], "if-none-match").unwrap(), "*");
        assert_eq!(records[1].body, b"synthetic-head-bytes");
        assert_eq!(header(&records[2], "if-match").unwrap(), "\"head-1\"");
        assert!(header(&records[4], "if-match").is_none());
        assert!(header(&records[4], "if-none-match").is_none());
    });
}

#[test]
fn a_head_survives_a_plain_replacement_and_is_readable_again() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let head = HeadBytes::new(b"published-head".to_vec()).unwrap();
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(200, &[("ETag", "\"head-9\"")], Vec::new()),
            reply(200, &[("ETag", "\"head-9\"")], b"published-head".to_vec()),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let locator = provider.head_locator(&handle).unwrap();
        let receipt = provider
            .replace_head(&handle, &locator, &head, &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("head"), 1024).unwrap();
        let read = provider
            .read_object(&handle, &locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        assert!(matches!(read, ReadReceipt::Body(body) if body.byte_length == 14));
        assert_eq!(
            std::fs::read(directory.path().join("head")).unwrap(),
            b"published-head"
        );
    });
}

#[test]
fn backblaze_reports_no_conditional_head_and_refuses_a_compare_exchange() {
    runtime().block_on(async {
        let b2 = reported_capabilities(&profiles::b2::PROFILE);
        assert!(!b2.atomic_create_head);
        assert!(!b2.conditional_head_update);
        assert!(!b2.conditional_get);
        assert!(b2.require(PublicationStrategy::Cas).is_err());
        assert!(b2.require(PublicationStrategy::Sequential).is_ok());

        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor()]);
        let (provider, handle) = opened(&test, "b2", &server).await;
        let locator = provider.head_locator(&handle).unwrap();
        assert_eq!(
            provider
                .compare_exchange_head(
                    &handle,
                    &locator,
                    &ExpectedHead::Absent,
                    &HeadBytes::new(b"head".to_vec()).unwrap(),
                    &Cancellation::default(),
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    });
}

#[test]
fn listing_pages_with_a_continuation_token_and_bounds_the_limit() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let test = dependencies();
        let first = listing("snapshots", &["snapshot-a", "snapshot-b"], Some("token-2"));
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(200, &[], first),
            reply(200, &[], listing("snapshots", &["snapshot-c"], None)),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        assert_eq!(
            provider
                .list_objects(&handle, Collection::Snapshots, None, 0, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(
            provider
                .list_objects(&handle, Collection::Snapshots, None, 1001, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        let page = provider
            .list_objects(&handle, Collection::Snapshots, None, 2, &cancel)
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].locator.object, "snapshots/snapshot-a");
        assert_eq!(
            page.objects[0].version,
            Some(VersionToken("\"snapshot-a-tag\"".into()))
        );
        assert_eq!(page.objects[0].byte_length, 64);
        assert!(page.objects[0].complete);
        assert_eq!(page.next_cursor.as_deref(), Some("token-2"));
        let last = provider
            .list_objects(
                &handle,
                Collection::Snapshots,
                page.next_cursor.as_deref(),
                2,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(last.objects.len(), 1);
        assert!(last.next_cursor.is_none());
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            line(&records[1]),
            "GET /synthetic/synthetic-bucket\
             ?list-type=2&max-keys=2&prefix=risunest%2Fsnapshots%2F HTTP/1.1"
        );
        assert_eq!(
            line(&records[2]),
            "GET /synthetic/synthetic-bucket\
             ?continuation-token=token-2&list-type=2&max-keys=2&prefix=risunest%2Fsnapshots%2F \
             HTTP/1.1"
        );
    });
}

#[test]
fn listing_rejects_a_nested_key_outside_the_collection_contract() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let test = dependencies();
        let nested = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult>\
             <Contents><Key>{PREFIX}/snapshots/deep/inner</Key><ETag>&quot;x&quot;</ETag>\
             <Size>1</Size></Contents></ListBucketResult>"
        );
        let server = WireServer::start(vec![one_descriptor(), reply(200, &[], nested.into_bytes())]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        assert_eq!(
            provider
                .list_objects(&handle, Collection::Snapshots, None, 2, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
    });
}

#[test]
fn a_documented_download_redirect_is_followed_without_carrying_authentication() {
    runtime().block_on(async {
        let spool = Spool::of(512);
        let cancel = Cancellation::default();
        let test = dependencies();
        let edge = WireServer::start(vec![reply(
            200,
            &[("ETag", "\"cdn\"")],
            spool.bytes.clone(),
        )]);
        let target = format!("http://127.0.0.1:{}/edge/object", edge.url.port().unwrap());
        let origin = WireServer::start(vec![
            one_descriptor(),
            reply(302, &[("Location", &target)], Vec::new()),
        ]);
        let (provider, handle) = opened(&test, "hf", &origin).await;
        let locator = RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: Some("packs".into()),
            object: "packs/pack-1".into(),
        };
        let mut sink = spool.sink("edge-received", 1024);
        let read = provider
            .read_object(
                &handle,
                &locator,
                Some(&VersionToken("\"ignored\"".into())),
                &mut sink,
                &cancel,
            )
            .await
            .unwrap();
        match read {
            ReadReceipt::Body(body) => {
                assert_eq!(body.byte_length, 512);
                // The edge entity tag is not the origin object version.
                assert!(body.version.is_none());
            }
            ReadReceipt::NotModified(_) => panic!("no conditional read is sent here"),
        }
        assert!(sink.is_verified());

        let records = origin.requests.lock().unwrap();
        assert_eq!(records.len(), 2);
        // Hugging Face documents no conditional GetObject, so none is sent.
        assert!(header(&records[1], "if-none-match").is_none());
        assert!(header(&records[1], "authorization").is_some());
        let hops = edge.requests.lock().unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(line(&hops[0]), "GET /edge/object HTTP/1.1");
        assert!(header(&hops[0], "authorization").is_none());
        assert!(header(&hops[0], "x-amz-date").is_none());
        assert!(header(&hops[0], "x-amz-content-sha256").is_none());
    });
}

#[test]
fn a_conditional_read_reports_not_modified_where_the_service_documents_it() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor(), reply(304, &[], Vec::new())]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let locator = provider.head_locator(&handle).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("unchanged"), 64).unwrap();
        let token = VersionToken("\"head-7\"".into());
        assert_eq!(
            provider
                .read_object(&handle, &locator, Some(&token), &mut sink, &cancel)
                .await
                .unwrap(),
            ReadReceipt::NotModified(token)
        );
        let records = server.requests.lock().unwrap();
        assert_eq!(header(&records[1], "if-none-match").unwrap(), "\"head-7\"");
    });
}

#[test]
fn a_multipart_session_uploads_every_part_and_completes_the_object() {
    runtime().block_on(async {
        let spool = Spool::of(2560);
        let cancel = Cancellation::default();
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(200, &[("ETag", "\"p1\"")], Vec::new()),
            reply(200, &[("ETag", "\"p2\"")], Vec::new()),
            reply(200, &[("ETag", "\"p3\"")], Vec::new()),
            reply(
                200,
                &[],
                b"<CompleteMultipartUploadResult><Bucket>synthetic-bucket</Bucket>\
                  <ETag>&quot;final-etag-3&quot;</ETag></CompleteMultipartUploadResult>"
                    .to_vec(),
            ),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let resume = seal_session(&test, "synthetic-upload", "risunest/packs/pack-1", 1024).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        let receipt = provider
            .create_object(&handle, &intent, &spool.source(), Some(&resume), &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.byte_length, 2560);
        assert_eq!(
            receipt.version,
            Some(VersionToken("\"final-etag-3\"".into()))
        );
        assert!(!receipt.checksum.unwrap().provider_verified);

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 5);
        for (index, (number, length)) in [(1u32, 1024usize), (2, 1024), (3, 512)].iter().enumerate()
        {
            let record = &records[index + 1];
            assert_eq!(
                line(record),
                format!(
                    "PUT /synthetic/synthetic-bucket/risunest/packs/pack-1\
                     ?partNumber={number}&uploadId=synthetic-upload HTTP/1.1"
                )
            );
            assert_eq!(
                header(record, "x-amz-content-sha256").unwrap(),
                sigv4::UNSIGNED_PAYLOAD
            );
            assert_eq!(record.body.len(), *length);
        }
        assert_eq!(
            line(&records[4]),
            "POST /synthetic/synthetic-bucket/risunest/packs/pack-1\
             ?uploadId=synthetic-upload HTTP/1.1"
        );
        assert_eq!(
            String::from_utf8(records[4].body.clone()).unwrap(),
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"p1\"</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>\"p2\"</ETag></Part>\
             <Part><PartNumber>3</PartNumber><ETag>\"p3\"</ETag></Part></CompleteMultipartUpload>"
        );
    });
}

#[test]
fn a_session_opens_only_above_the_threshold_and_seals_its_own_state() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(
                200,
                &[],
                b"<InitiateMultipartUploadResult><Bucket>synthetic-bucket</Bucket>\
                  <Key>risunest/packs/pack-big</Key><UploadId>synthetic-upload-id</UploadId>\
                  </InitiateMultipartUploadResult>"
                    .to_vec(),
            ),
        ]);
        let (provider, handle) = opened(&test, "hf", &server).await;
        let mut small = ObjectIntent {
            repository_id: handle.repository_id.clone(),
            job_id: "synthetic-job".into(),
            object_id: "pack-small".into(),
            role: ObjectRole::Pack,
            byte_length: 1024,
            sha256: risunest_sync_wire::hash(b"small"),
        };
        assert!(provider
            .begin_upload(&handle, &small, &cancel)
            .await
            .unwrap()
            .is_none());
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        small.object_id = "pack-big".into();
        small.byte_length = 100 * 1024 * 1024;
        let resume = provider
            .begin_upload(&handle, &small, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resume.confirmed_offset, 0);
        // Hugging Face documents a seven day multipart lifetime.
        assert_eq!(resume.expires_at_ms, Some(NOW_MS + 7 * 24 * 60 * 60 * 1000));
        let sealed = test.vault.contents(&resume.sealed_state.0).unwrap();
        let sealed: serde_json::Value = serde_json::from_slice(&sealed).unwrap();
        assert_eq!(sealed["uploadId"], "synthetic-upload-id");
        assert_eq!(sealed["key"], "risunest/packs/pack-big");
        assert_eq!(sealed["partSize"], 64 * 1024 * 1024u64);
        assert_eq!(sealed["parts"], serde_json::json!([]));
        let records = server.requests.lock().unwrap();
        assert_eq!(
            line(&records[1]),
            "POST /synthetic/synthetic-bucket/risunest/packs/pack-big?uploads= HTTP/1.1"
        );
    });
}

#[test]
fn reconcile_separates_completion_conflict_expiry_and_a_confirmed_offset() {
    runtime().block_on(async {
        let spool = Spool::of(2560);
        let cancel = Cancellation::default();
        let parts = |entries: &[(u32, usize)]| {
            let mut body = String::from("<ListPartsResult><Bucket>synthetic-bucket</Bucket>");
            for (number, size) in entries {
                body.push_str(&format!(
                    "<Part><PartNumber>{number}</PartNumber><Size>{size}</Size>\
                     <ETag>&quot;p{number}&quot;</ETag></Part>"
                ));
            }
            body.push_str("</ListPartsResult>");
            body.into_bytes()
        };

        // The final object exists with the intended length.
        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor(), metadata(2560, "\"done\"")]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let resume = seal_session(&test, "synthetic-upload", "risunest/packs/pack-1", 1024).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        match provider
            .reconcile_upload(&handle, &intent, Some(&resume), &cancel)
            .await
            .unwrap()
        {
            UploadResolution::Complete(receipt) => {
                assert_eq!(receipt.byte_length, 2560);
                assert_eq!(receipt.locator.object, "packs/pack-1");
            }
            _ => panic!("expected a completed object"),
        }

        // Another length under the same identity is a conflict.
        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor(), metadata(999, "\"other\"")]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let resume = seal_session(&test, "synthetic-upload", "risunest/packs/pack-1", 1024).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        assert!(matches!(
            provider
                .reconcile_upload(&handle, &intent, Some(&resume), &cancel)
                .await
                .unwrap(),
            UploadResolution::Conflict
        ));

        // A session the service no longer knows has to restart.
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(404, &[], Vec::new()),
            reply(
                404,
                &[],
                b"<Error><Code>NoSuchUpload</Code></Error>".to_vec(),
            ),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let resume = seal_session(&test, "synthetic-upload", "risunest/packs/pack-1", 1024).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        assert!(matches!(
            provider
                .reconcile_upload(&handle, &intent, Some(&resume), &cancel)
                .await
                .unwrap(),
            UploadResolution::RestartRequired
        ));

        // Only an unbroken run of confirmed parts counts, and the rest resumes.
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(404, &[], Vec::new()),
            reply(200, &[], parts(&[(1, 1024), (2, 1024), (4, 512)])),
            reply(200, &[("ETag", "\"p3\"")], Vec::new()),
            reply(
                200,
                &[],
                b"<CompleteMultipartUploadResult><ETag>&quot;final&quot;</ETag>\
                  </CompleteMultipartUploadResult>"
                    .to_vec(),
            ),
        ]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let resume = seal_session(&test, "synthetic-upload", "risunest/packs/pack-1", 1024).await;
        let intent = spool.intent(&handle, "pack-1", ObjectRole::Pack);
        let resumed = match provider
            .reconcile_upload(&handle, &intent, Some(&resume), &cancel)
            .await
            .unwrap()
        {
            UploadResolution::Resumable(state) => state,
            _ => panic!("expected a resumable session"),
        };
        assert_eq!(resumed.confirmed_offset, 2048);
        let sealed = test.vault.contents(&resumed.sealed_state.0).unwrap();
        let sealed: serde_json::Value = serde_json::from_slice(&sealed).unwrap();
        assert_eq!(sealed["parts"].as_array().unwrap().len(), 2);
        assert_eq!(sealed["parts"][1]["etag"], "\"p2\"");
        let receipt = provider
            .create_object(&handle, &intent, &spool.source(), Some(&resumed), &cancel)
            .await
            .unwrap();
        assert!(receipt.complete);
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(
            line(&records[2]),
            "GET /synthetic/synthetic-bucket/risunest/packs/pack-1\
             ?max-parts=1000&uploadId=synthetic-upload HTTP/1.1"
        );
        assert_eq!(
            line(&records[3]),
            "PUT /synthetic/synthetic-bucket/risunest/packs/pack-1\
             ?partNumber=3&uploadId=synthetic-upload HTTP/1.1"
        );
        assert_eq!(records[3].body.len(), 512);
        assert_eq!(
            String::from_utf8(records[4].body.clone()).unwrap(),
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"p1\"</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>\"p2\"</ETag></Part>\
             <Part><PartNumber>3</PartNumber><ETag>\"p3\"</ETag></Part></CompleteMultipartUpload>"
        );
    });
}

#[test]
fn documented_failure_statuses_map_to_provider_kinds_without_a_server_message() {
    runtime().block_on(async {
        let body = b"<Error><Code>SignatureDoesNotMatch</Code><Message>synthetic detail</Message>\
                     </Error>"
            .to_vec();
        let read = |provider: Arc<dyn Provider>, handle: RepositoryHandle| async move {
            let directory = tempfile::tempdir().unwrap();
            let mut sink = SpoolSink::create(&directory.path().join("body"), 64).unwrap();
            provider
                .read_object(
                    &handle,
                    &provider.head_locator(&handle).unwrap(),
                    None,
                    &mut sink,
                    &Cancellation::default(),
                )
                .await
                .unwrap_err()
        };

        for (profile, status, headers, expected) in [
            (
                "r2",
                403,
                vec![],
                ProviderError {
                    kind: ErrorKind::Unauthorized,
                    http_status: Some(403),
                    retry_at_ms: None,
                },
            ),
            (
                "r2",
                404,
                vec![],
                ProviderError {
                    kind: ErrorKind::NotFound,
                    http_status: Some(404),
                    retry_at_ms: None,
                },
            ),
            (
                "r2",
                429,
                vec![("Retry-After", "30")],
                ProviderError {
                    kind: ErrorKind::RateLimited,
                    http_status: Some(429),
                    retry_at_ms: Some(NOW_MS + 30_000),
                },
            ),
            (
                "r2",
                503,
                vec![("Retry-After", "5")],
                ProviderError {
                    kind: ErrorKind::Transient,
                    http_status: Some(503),
                    retry_at_ms: Some(NOW_MS + 5_000),
                },
            ),
            (
                "b2",
                503,
                vec![("Retry-After", "5")],
                ProviderError {
                    kind: ErrorKind::RateLimited,
                    http_status: Some(503),
                    retry_at_ms: Some(NOW_MS + 5_000),
                },
            ),
            (
                "r2",
                507,
                vec![],
                ProviderError {
                    kind: ErrorKind::StorageFull,
                    http_status: Some(507),
                    retry_at_ms: None,
                },
            ),
        ] {
            let test = dependencies();
            let server = WireServer::start(vec![
                one_descriptor(),
                reply(status, &headers, body.clone()),
            ]);
            let (provider, handle) = opened(&test, profile, &server).await;
            assert_eq!(read(provider, handle).await, expected, "{profile} {status}");
        }
    });
}

#[test]
fn a_foreign_handle_or_locator_never_reaches_the_service() {
    runtime().block_on(async {
        let cancel = Cancellation::default();
        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor()]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("body"), 64).unwrap();

        let foreign = RemoteLocator {
            connection_identity: "s3/r2/https://other.invalid/other/root".into(),
            collection: None,
            object: "head".into(),
        };
        assert_eq!(
            provider
                .read_object(&handle, &foreign, None, &mut sink, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        let stray = RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: None,
            object: "packs/pack-1".into(),
        };
        assert_eq!(
            provider
                .replace_head(
                    &handle,
                    &stray,
                    &HeadBytes::new(b"head".to_vec()).unwrap(),
                    &cancel
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        let alien = fake::repository();
        assert!(provider.head_locator(&alien).is_err());
        assert_eq!(
            provider
                .list_objects(&alien, Collection::Snapshots, None, 10, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    });
}

#[test]
fn cancellation_during_a_body_stops_the_transfer() {
    runtime().block_on(async {
        let test = dependencies();
        let server = WireServer::start(vec![one_descriptor(), Reply::DelayedBody]);
        let (provider, handle) = opened(&test, "r2", &server).await;
        let locator = provider.head_locator(&handle).unwrap();
        let cancel = Cancellation::default();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("partial"), 100).unwrap();
        let read = provider.read_object(&handle, &locator, None, &mut sink, &cancel);
        let trigger = async {
            while server.requests.lock().unwrap().len() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures::future::join(read, trigger),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
        assert!(!sink.is_verified());
    });
}

/// A body shorter than the declared length must not be accepted as an object.
struct ShortBody(AtomicUsize);
impl HttpTransport for ShortBody {
    fn send<'a>(&'a self, _: HttpRequest, _: &'a Cancellation) -> ProviderFuture<'a, HttpResponse> {
        Box::pin(async move {
            let (body, declared) = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                let listing = listing("descriptors", &["descriptor-1"], None);
                let length = listing.len();
                (listing, length)
            } else {
                (vec![7u8; 10], 2048)
            };
            let mut headers = BTreeMap::new();
            headers.insert("content-length".to_owned(), declared.to_string());
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::pin(std::io::Cursor::new(body)),
            })
        })
    }
}

#[test]
fn a_body_shorter_than_its_declared_length_is_corrupt() {
    runtime().block_on(async {
        let test = with_transport(Arc::new(ShortBody(AtomicUsize::new(0))), vault(), NOW_MS);
        let (provider, handle, _) = open(
            &test,
            "r2",
            "https://synthetic-account.r2.cloudflarestorage.com",
            OpenMode::Existing,
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut sink = SpoolSink::create(&directory.path().join("short"), 4096).unwrap();
        assert_eq!(
            provider
                .read_object(
                    &handle,
                    &provider.head_locator(&handle).unwrap(),
                    None,
                    &mut sink,
                    &Cancellation::default()
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert!(!sink.is_verified());
    });
}

#[test]
fn deleting_addresses_one_key_folds_404_and_refuses_the_head_and_descriptors() {
    runtime().block_on(async {
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(204, &[], Vec::new()),
            reply(404, &[], Vec::new()),
        ]);
        let (provider, handle) = opened(&test, "generic", &server).await;
        let cancel = Cancellation::default();
        let target = RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: Some("packs".into()),
            object: "packs/pack-1".into(),
        };
        provider
            .delete_object(&handle, &target, &cancel)
            .await
            .unwrap();
        // A key that is not there is already in the state the caller wanted.
        provider
            .delete_object(&handle, &target, &cancel)
            .await
            .unwrap();

        let refused = |object: &str, collection: Option<&str>| RemoteLocator {
            connection_identity: handle.connection_identity.clone(),
            collection: collection.map(str::to_owned),
            object: object.to_owned(),
        };
        for locator in [
            provider.head_locator(&handle).unwrap(),
            refused("descriptors/descriptor-1", None),
            refused("packs/pack-1", Some("snapshots")),
            refused("elsewhere/pack-1", None),
            refused("packs/", None),
            refused("packs/nested/pack-1", None),
        ] {
            assert_eq!(
                provider
                    .delete_object(&handle, &locator, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Unsupported
            );
        }
        let foreign = RemoteLocator {
            connection_identity: "s3/generic/https://other.invalid/other/root".into(),
            collection: Some("packs".into()),
            object: "packs/pack-1".into(),
        };
        assert_eq!(
            provider
                .delete_object(&handle, &foreign, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );

        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert!(line(&records[1]).starts_with("DELETE "));
        assert!(line(&records[1]).ends_with(&format!("/{BUCKET}/{PREFIX}/packs/pack-1 HTTP/1.1")));
        assert_eq!(
            header(&records[1], "x-amz-content-sha256").as_deref(),
            Some(EMPTY_HASH)
        );
        drop(records);
    });
}

#[test]
fn cleanup_capability_follows_the_preset() {
    for preset in ["r2", "aws", "generic", "b2", "hf"] {
        assert!(reported_capabilities(profiles::lookup(preset).unwrap())
            .require_cleanup()
            .is_ok());
    }
}

#[test]
fn capabilities_match_every_preset() {
    let presets: [(&str, &Profile); 5] = [
        ("aws", &profiles::aws::PROFILE),
        ("r2", &profiles::r2::PROFILE),
        ("b2", &profiles::b2::PROFILE),
        ("hf", &profiles::hf::PROFILE),
        ("generic", &profiles::generic::PROFILE),
    ];
    for (id, profile) in presets {
        let reported = reported_capabilities(profile);
        assert!(reported.immutable_create, "{id}");
        assert!(reported.direct_complete_read, "{id}");
        assert!(reported.stable_head_replace, "{id}");
        assert!(reported.head_read_after_write, "{id}");
        assert!(reported.head_retry_control, "{id}");
        assert!(reported.snapshot_discovery, "{id}");
        assert_eq!(reported.sdk_overhead_bytes, 0, "{id}");
        assert_eq!(reported.upload_alignment, 64 * 1024 * 1024, "{id}");
        assert!(!reported.range, "{id}");
        assert!(reported.resumable_upload, "{id}");
        assert!(
            reported.require(PublicationStrategy::Sequential).is_ok(),
            "{id}"
        );
        assert_eq!(
            reported.require(PublicationStrategy::Cas).is_ok(),
            matches!(id, "r2" | "hf"),
            "{id}"
        );
        assert_eq!(
            reported.max_stored_bytes,
            Some(10_000 * 64 * 1024 * 1024),
            "{id}"
        );
        assert_eq!(
            reported.conditional_get,
            matches!(id, "r2" | "generic" | "aws"),
            "{id}"
        );
    }
}

/// Leases are their own key folder, so an enumeration of them can never return
/// a snapshot, a pack or the descriptor that proves the root.
#[test]
fn the_lease_collection_is_its_own_key_folder_and_is_removable() {
    runtime().block_on(async {
        let tag = "0123456789abcdef0123456789abcdef";
        let name = lease_object_id(LeaseKind::Cleanup, tag).unwrap();
        let test = dependencies();
        let server = WireServer::start(vec![
            one_descriptor(),
            reply(200, &[], listing("leases", &[&name], None)),
            reply(204, &[], Vec::new()),
        ]);
        let (provider, handle) = opened(&test, "generic", &server).await;
        let cancel = Cancellation::default();
        let page = provider
            .list_objects(&handle, Collection::Leases, None, 10, &cancel)
            .await
            .unwrap();
        assert_eq!(
            page.objects
                .iter()
                .map(|object| object.locator.object.as_str())
                .collect::<Vec<_>>(),
            vec![format!("leases/{name}")]
        );
        provider
            .delete_object(&handle, &page.objects[0].locator, &cancel)
            .await
            .unwrap();
        let records = server.requests.lock().unwrap();
        assert!(line(&records[1]).contains(&format!("prefix={PREFIX}%2Fleases%2F")));
        assert!(line(&records[2]).ends_with(&format!("/{PREFIX}/leases/{name} HTTP/1.1")));
    });
}
