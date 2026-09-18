//! Explicit extension of the synthetic owner fixture, never an installed profile.
use super::*;
use std::sync::atomic::AtomicU64;

#[test]
#[ignore = "Requires a settled retained synthetic 100k-owner report"]
fn server_sync_retained_owner_library_500_character_gate() {
    let report =
        std::env::var_os("RISUNEST_SYNTHETIC_OWNER_REPORT").expect("synthetic report required");
    let report: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    let marker = "risunest-sync-server-synthetic-owner-v1";
    assert_eq!(report["marker"], marker);
    let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    let roots: Vec<_> = ["server", "first", "second"]
        .into_iter()
        .map(|key| {
            let root = fs::canonicalize(report[key].as_str().unwrap()).unwrap();
            assert!(
                root.starts_with(&temp) && root != temp,
                "fixture must be a private temporary directory"
            );
            assert_eq!(
                fs::read_to_string(root.join(".risunest-synthetic-owner-fixture")).unwrap(),
                marker
            );
            root
        })
        .collect();
    assert_eq!(
        roots
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    // The server owner lock rejects concurrent use by the originating gate.
    let server = Arc::new(Store::open(&roots[0]).unwrap());
    let mut first = PersistentStore::open(&roots[1]).unwrap();
    let mut second = PersistentStore::open(&roots[2]).unwrap();
    for store in [&first, &second] {
        let status = store.server_status().unwrap();
        assert!(!status.full_scan && !status.operation_pending && status.dirty_records == 0);
        let generation = active_generation(&store.connection).unwrap();
        let owner_count: i64 = store.connection.query_row("SELECT json_array_length(detail,'$.additionalAssets') FROM characters WHERE generation=?1 AND character_id='char-a'",[generation],|r|r.get(0)).unwrap();
        assert_eq!(owner_count, 100_000);
        let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).unwrap();
        // Identify the fixture here; the warm gate below verifies every body.
        for index in [0, 50_000, 99_999] {
            let bytes = format!("synthetic unique asset body {index:016}");
            assert_eq!(
                cas.read_object(&risunest_sync_wire::hash(bytes.as_bytes()))
                    .unwrap()
                    .unwrap(),
                bytes.as_bytes()
            );
        }
    }
    let config = first.server_config().unwrap().unwrap();
    assert_eq!(
        second.server_config().unwrap().unwrap().endpoint,
        config.endpoint
    );
    let endpoint = reqwest::Url::parse(&config.endpoint).unwrap();
    assert_eq!(endpoint.scheme(), "http");
    assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind((
            "127.0.0.1",
            endpoint.port().unwrap(),
        )))
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    let counted = CountedListener {
        listener,
        bytes: counter.clone(),
    };
    let task = runtime.spawn(async move {
        axum::serve(
            counted,
            http::router(server).layer(axum::middleware::from_fn(trace_request)),
        )
        .await
        .unwrap();
    });
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    let generation = active_generation(&first.connection).unwrap();
    let raw: String = first
        .connection
        .query_row(
            "SELECT detail FROM characters WHERE generation=?1 AND character_id='char-a'",
            [generation],
            |r| r.get(0),
        )
        .unwrap();
    let mut detail: Value = serde_json::from_str(&raw).unwrap();
    // Never alternate between already cached historical targets: each rerun
    // must exercise creation and transfer of a new changed representation.
    let name = format!("{:06}", first.revision().unwrap());
    assert_eq!(name.len(), 6);
    assert_ne!(detail["additionalAssets"][50_000][0], name);
    detail["additionalAssets"][50_000][0] = json!(name);
    let entries: Vec<_> = detail["additionalAssets"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, tuple)| OwnerManifestEntry {
            tuple: std::array::from_fn(|i| tuple[i].as_str().unwrap().to_owned()),
            payload_hash: Some(
                sha2::Sha256::digest(format!("synthetic unique asset body {index:016}").as_bytes())
                    .into(),
            ),
        })
        .collect();
    let cas = crate::asset_repository::PayloadCas::new(first.repository_root()).unwrap();
    let manifest = cas
        .prepare_bytes(&encode_owner_manifest(&entries).unwrap())
        .unwrap();
    first
        .commit(&WorkingSetCommit {
            character_details: Some(vec![detail]),
            asset_owner_heads: Some(vec![AssetOwnerHead::present(
                AssetOwnerLocator::CharacterAdditionalAssets {
                    character_id: "char-a".into(),
                },
                manifest.content_hash,
                100_000,
            )]),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    counter.store(0, AtomicOrdering::Relaxed);
    let started = std::time::Instant::now();
    eprintln!("retained 100k warm upload starting");
    assert_eq!(settle(&mut first).phase, "idle");
    let upload = counter.swap(0, AtomicOrdering::Relaxed);
    let upload_ms = started.elapsed().as_millis();
    eprintln!("retained 100k warm upload complete: {upload} HTTP bytes in {upload_ms} ms");
    let started = std::time::Instant::now();
    eprintln!("retained 100k warm download starting");
    assert_eq!(settle(&mut second).phase, "idle");
    let download = counter.load(AtomicOrdering::Relaxed);
    eprintln!("retained 100k D=6 upload={upload} download={download} upload_ms={upload_ms} download_ms={}", started.elapsed().as_millis());
    let generation = active_generation(&second.connection).unwrap();
    let received: String = second.connection.query_row("SELECT json_extract(detail,'$.additionalAssets[50000][0]') FROM characters WHERE generation=?1 AND character_id='char-a'", [generation], |r|r.get(0)).unwrap();
    assert_eq!(received, name);
    let cas = crate::asset_repository::PayloadCas::new(second.repository_root()).unwrap();
    for index in 0..100_000 {
        let bytes = format!("synthetic unique asset body {index:016}");
        assert_eq!(
            cas.read_object(&risunest_sync_wire::hash(bytes.as_bytes()))
                .unwrap()
                .unwrap(),
            bytes.as_bytes()
        );
    }
    eprintln!("retained 100k exact payloads verified");
    // The 16 KiB target is informational for this extreme owner fixture.
    // Large owner metadata has a separate ceiling; ordinary budgets remain unchanged.
    assert!(upload <= 32_768 + 6, "owner upload {upload}");
    assert!(download <= 32_768 + 6, "owner download {download}");
    let (_seed_dir, _seed_store, database) = open_fixture();
    let generation = active_generation(&first.connection).unwrap();
    let existing: i64 = first
        .connection
        .query_row(
            "SELECT count(*) FROM characters WHERE generation=?1",
            [generation],
            |r| r.get(0),
        )
        .unwrap();
    assert!(existing <= 500);
    for index in existing..500 {
        let mut character = database["characters"][1].clone();
        character["chaId"] = json!(format!("synthetic-scale-{index:04}"));
        first
            .commit(&WorkingSetCommit {
                add_character: Some(character),
                ..empty_working_set_commit(first.revision().unwrap())
            })
            .unwrap();
    }
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    for store in [&first, &second] {
        let generation = active_generation(&store.connection).unwrap();
        let count: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM characters WHERE generation=?1",
                [generation],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 500);
    }
    let mut root = first.read_root(None).unwrap().value;
    root["username"] = json!("scaled");
    first
        .commit(&WorkingSetCommit {
            root: Some(root),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    counter.store(0, AtomicOrdering::Relaxed);
    assert_eq!(settle(&mut first).phase, "idle");
    let upload = counter.swap(0, AtomicOrdering::Relaxed);
    assert_eq!(settle(&mut second).phase, "idle");
    let download = counter.load(AtomicOrdering::Relaxed);
    assert_eq!(second.read_root(None).unwrap().value["username"], "scaled");
    assert!(upload <= 16_384 + 6, "scaled upload {upload}");
    assert!(download <= 16_384 + 6, "scaled download {download}");
    eprintln!("500 characters + 100000 verified assets, warm root D=6: upload={upload}, download={download}");
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}

async fn trace_request(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::body::HttpBody;
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let input = request.body().size_hint().exact();
    if path == "/uploads/frames" {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, risunest_sync_wire::transfer::MAX_BATCH_BYTES)
            .await
            .unwrap();
        for frame in risunest_sync_wire::transfer::decode(&bytes).unwrap() {
            match frame {
                risunest_sync_wire::transfer::Frame::Full(bytes) => {
                    eprintln!("synthetic full frame bytes={}", bytes.len())
                }
                risunest_sync_wire::transfer::Frame::Delta(recipe) => eprintln!(
                    "synthetic delta frame target={} recipe={} bases={:?}",
                    recipe.target_size,
                    recipe.encode().unwrap().len(),
                    recipe.bases.iter().map(|b| b.size).collect::<Vec<_>>()
                ),
                _ => panic!("unexpected upload frame"),
            }
        }
        request = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    }
    let response = next.run(request).await;
    eprintln!(
        "synthetic HTTP {method} {path}: {input:?} -> {} {:?}",
        response.status().as_u16(),
        response.body().size_hint().exact()
    );
    response
}
