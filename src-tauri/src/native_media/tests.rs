use super::{decode_physical_key, respond};
use serde_json::json;
use std::{fs, path::Path};
use tauri::http::{header, Method, Request, StatusCode};
use tempfile::TempDir;

fn hex(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn request(method: Method, physical_key: &str) -> Request<Vec<u8>> {
    Request::builder()
        .method(method)
        .uri(format!("http://risuasset.localhost/{}", hex(physical_key)))
        .body(Vec::new())
        .unwrap()
}

fn write_blob(root: &Path, logical_key: &str, bytes: &[u8], mime: &str) {
    let physical_key = if logical_key.starts_with("assets/") {
        logical_key.to_owned()
    } else {
        format!("blobstore/inlays/{}.bin", hex(logical_key))
    };
    let payload = root.join(physical_key.replace('/', std::path::MAIN_SEPARATOR_STR));
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(payload, bytes).unwrap();

    let kind = if logical_key.starts_with("assets/") {
        "asset"
    } else {
        "inlay"
    };
    let mut metadata = json!({
        "key": logical_key,
        "kind": kind,
        "size": bytes.len(),
        "mime": mime,
        "name": "fixture",
        "ext": "bin"
    });
    if kind == "inlay" {
        metadata["inlayType"] = json!("image");
    }
    let metadata_path = root
        .join("blobstore/metadata")
        .join(format!("{}.json", hex(logical_key)));
    fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    fs::write(metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
}

#[test]
fn maps_only_supported_physical_keys_without_traversal() {
    assert_eq!(
        decode_physical_key(&format!(
            "http://risuasset.localhost/{}",
            hex("assets/folder/photo.png")
        )),
        Some("assets/folder/photo.png".to_owned())
    );
    assert_eq!(
        decode_physical_key(&format!(
            "risuasset://localhost/{}",
            hex("blobstore/inlays/616263.bin")
        )),
        Some("blobstore/inlays/616263.bin".to_owned())
    );

    for invalid in [
        "assets/../secret",
        "assets//secret",
        "assets/a\\secret",
        "assets/C:secret",
        "blobstore/inlays/ABCDEF.bin",
        "blobstore/inlays/abc.bin",
        "blobstore/inlays/gg.bin",
        "blobstore/metadata/6162.json",
        "coldstorage/value",
        "/absolute",
    ] {
        assert_eq!(
            decode_physical_key(&format!("http://risuasset.localhost/{}", hex(invalid))),
            None,
            "accepted {invalid}"
        );
    }
    assert_eq!(
        decode_physical_key("http://risuasset.localhost/not-hex"),
        None
    );
    assert_eq!(
        decode_physical_key(&format!(
            "https://example.invalid/{}",
            hex("assets/photo.png")
        )),
        None
    );
}

#[test]
fn serves_get_and_head_with_validated_metadata_and_media_headers() {
    let temp = TempDir::new().unwrap();
    write_blob(
        temp.path(),
        "assets/folder/photo.bin",
        b"abcdef",
        "image/custom",
    );

    let get = respond(temp.path(), request(Method::GET, "assets/folder/photo.bin"));
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.body(), b"abcdef");
    assert_eq!(get.headers()[header::CONTENT_TYPE], "image/custom");
    assert_eq!(get.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(get.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(get.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert!(get.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS]
        .to_str()
        .unwrap()
        .contains("Content-Range"));
    assert!(get.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .starts_with("W/\""));

    let head = respond(
        temp.path(),
        request(Method::HEAD, "assets/folder/photo.bin"),
    );
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.body().is_empty());
    assert_eq!(head.headers()[header::CONTENT_LENGTH], "6");
    assert_eq!(head.headers()[header::CONTENT_TYPE], "image/custom");
}

#[test]
fn serves_all_single_range_forms_and_limits_each_body() {
    let temp = TempDir::new().unwrap();
    write_blob(
        temp.path(),
        "assets/data.bin",
        b"0123456789",
        "application/x-fixture",
    );
    for (range, expected, content_range) in [
        ("bytes=2-5", b"2345".as_slice(), "bytes 2-5/10"),
        ("bytes=7-", b"789".as_slice(), "bytes 7-9/10"),
        ("bytes=-3", b"789".as_slice(), "bytes 7-9/10"),
    ] {
        let mut req = request(Method::GET, "assets/data.bin");
        req.headers_mut()
            .insert(header::RANGE, range.parse().unwrap());
        let response = respond(temp.path(), req);
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.body(), expected);
        assert_eq!(response.headers()[header::CONTENT_RANGE], content_range);
    }

    let large = vec![7; 1024 * 1024 + 9];
    write_blob(
        temp.path(),
        "assets/large.bin",
        &large,
        "application/octet-stream",
    );
    let response = respond(temp.path(), request(Method::GET, "assets/large.bin"));
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body(), &large);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "1048585");
    assert!(!response.headers().contains_key(header::CONTENT_RANGE));

    let head = respond(temp.path(), request(Method::HEAD, "assets/large.bin"));
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.body().is_empty());
    assert_eq!(head.headers()[header::CONTENT_LENGTH], "1048585");
    assert!(!head.headers().contains_key(header::CONTENT_RANGE));

    let mut ranged = request(Method::GET, "assets/large.bin");
    ranged
        .headers_mut()
        .insert(header::RANGE, "bytes=0-".parse().unwrap());
    let response = respond(temp.path(), ranged);
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.body().len(), 1024 * 1024);
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        "bytes 0-1048575/1048585"
    );
}

#[test]
fn returns_conditional_and_error_statuses() {
    let temp = TempDir::new().unwrap();
    write_blob(
        temp.path(),
        "assets/data.bin",
        b"0123456789",
        "application/octet-stream",
    );

    let initial = respond(temp.path(), request(Method::GET, "assets/data.bin"));
    let mut conditional = request(Method::GET, "assets/data.bin");
    conditional.headers_mut().insert(
        header::IF_NONE_MATCH,
        initial.headers()[header::ETAG].clone(),
    );
    assert_eq!(
        respond(temp.path(), conditional).status(),
        StatusCode::NOT_MODIFIED
    );

    let mut invalid_range = request(Method::GET, "assets/data.bin");
    invalid_range
        .headers_mut()
        .insert(header::RANGE, "bytes=99-100".parse().unwrap());
    let invalid_range = respond(temp.path(), invalid_range);
    assert_eq!(invalid_range.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(invalid_range.headers()[header::CONTENT_RANGE], "bytes */10");

    let mut multiple_ranges = request(Method::GET, "assets/data.bin");
    multiple_ranges
        .headers_mut()
        .insert(header::RANGE, "bytes=0-1,3-4".parse().unwrap());
    assert_eq!(
        respond(temp.path(), multiple_ranges).status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    assert_eq!(
        respond(temp.path(), request(Method::POST, "assets/data.bin")).status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        respond(temp.path(), request(Method::GET, "assets/missing.bin")).status(),
        StatusCode::NOT_FOUND
    );

    fs::write(
        temp.path().join("blobstore/metadata").join(format!("{}.json", hex("assets/data.bin"))),
        br#"{"key":"assets/other.bin","kind":"asset","size":10,"mime":"text/plain","name":"bad","ext":"bin"}"#,
    ).unwrap();
    assert_eq!(
        respond(temp.path(), request(Method::GET, "assets/data.bin")).status(),
        StatusCode::NOT_FOUND
    );
}

#[test]
fn ignores_thumbnail_queries_and_serves_the_original_without_creating_a_cache() {
    let temp = TempDir::new().unwrap();
    let original = b"original-image-payload";
    write_blob(temp.path(), "assets/tiny.png", original, "image/custom");

    let mut req = request(Method::GET, "assets/tiny.png");
    *req.uri_mut() = format!(
        "http://risuasset.localhost/{}?thumb=128",
        hex("assets/tiny.png")
    )
    .parse()
    .unwrap();
    let response = respond(temp.path(), req);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/custom");
    assert_eq!(response.body(), original);
    assert!(!temp.path().join("blobstore/thumbnails").exists());
}
