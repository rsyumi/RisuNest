use super::{
    decode_physical_key, recover_inlay_writes, respond, write_inlay_image, InlayImageMetadata,
};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use serde_json::json;
use std::{fs, io::Cursor, path::Path};
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

fn encoded_fixture(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |x, y| {
        Rgba([(x * 17) as u8, (y * 29) as u8, 91, 255])
    }));
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, format).unwrap();
    output.into_inner()
}

#[test]
fn writes_png_inlay_as_full_dimension_webp_with_truthful_metadata() {
    let temp = TempDir::new().unwrap();
    let source = encoded_fixture(ImageFormat::Png, 7, 3);

    let metadata = write_inlay_image(temp.path(), "new-image", &source, "photo.png").unwrap();

    assert_eq!(metadata.key, "new-image");
    assert_eq!(metadata.kind, "inlay");
    assert_eq!(metadata.inlay_type, "image");
    assert_eq!(metadata.mime, "image/webp");
    assert_eq!(metadata.ext, "webp");
    assert_eq!(metadata.name, "photo.png");
    assert_eq!((metadata.width, metadata.height), (7, 3));

    let payload_path = temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{}.bin", hex("new-image")));
    let payload = fs::read(payload_path).unwrap();
    let source_rgba = image::load_from_memory_with_format(&source, ImageFormat::Png)
        .unwrap()
        .to_rgba8();
    let expected = webp::Encoder::from_rgba(
        source_rgba.as_raw(),
        source_rgba.width(),
        source_rgba.height(),
    )
    .encode(85.0);
    assert_eq!(payload.as_slice(), std::ops::Deref::deref(&expected));
    let decoded = image::load_from_memory_with_format(&payload, ImageFormat::WebP).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (7, 3));

    let metadata_path = temp
        .path()
        .join("blobstore/metadata")
        .join(format!("{}.json", hex("new-image")));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(metadata_path).unwrap()).unwrap(),
        serde_json::to_value(metadata).unwrap()
    );
}

fn with_exif_orientation(jpeg: Vec<u8>, orientation: u8) -> Vec<u8> {
    assert!(jpeg.starts_with(&[0xff, 0xd8]));
    let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend_from_slice(&[orientation, 0, 0, 0, 0, 0, 0, 0]);
    let length = (exif.len() + 2) as u16;
    let mut oriented = Vec::with_capacity(jpeg.len() + exif.len() + 4);
    oriented.extend_from_slice(&jpeg[..2]);
    oriented.extend_from_slice(&[0xff, 0xe1]);
    oriented.extend_from_slice(&length.to_be_bytes());
    oriented.extend_from_slice(&exif);
    oriented.extend_from_slice(&jpeg[2..]);
    oriented
}

#[test]
fn records_display_dimensions_after_applying_jpeg_orientation() {
    let temp = TempDir::new().unwrap();
    let source = with_exif_orientation(encoded_fixture(ImageFormat::Jpeg, 8, 3), 6);

    let metadata = write_inlay_image(temp.path(), "oriented", &source, "oriented.jpg").unwrap();

    assert_eq!((metadata.width, metadata.height), (3, 8));
}

#[test]
fn writes_jpeg_and_webp_sources_once_at_quality_85_without_resizing() {
    for (id, source, width, height) in [
        ("jpeg", encoded_fixture(ImageFormat::Jpeg, 9, 4), 9, 4),
        ("webp", encoded_fixture(ImageFormat::WebP, 5, 8), 5, 8),
    ] {
        let temp = TempDir::new().unwrap();

        let metadata = write_inlay_image(temp.path(), id, &source, "source").unwrap();
        let payload = fs::read(
            temp.path()
                .join("blobstore/inlays")
                .join(format!("{}.bin", hex(id))),
        )
        .unwrap();
        let decoded = image::load_from_memory_with_format(&payload, ImageFormat::WebP).unwrap();

        assert_eq!((metadata.width, metadata.height), (width, height));
        assert_eq!((decoded.width(), decoded.height()), (width, height));
    }
}

#[test]
fn rejects_unsupported_gif_avif_and_corrupt_new_images_without_mutating_prior_data() {
    let temp = TempDir::new().unwrap();
    let original = encoded_fixture(ImageFormat::Png, 3, 2);
    let original_metadata =
        write_inlay_image(temp.path(), "stable", &original, "stable.png").unwrap();
    let payload_path = temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{}.bin", hex("stable")));
    let metadata_path = temp
        .path()
        .join("blobstore/metadata")
        .join(format!("{}.json", hex("stable")));
    let prior_payload = fs::read(&payload_path).unwrap();
    let prior_metadata = fs::read(&metadata_path).unwrap();
    let gif = encoded_fixture(ImageFormat::Gif, 2, 1);
    let avif = b"\0\0\0\x18ftypavif\0\0\0\0avifmif1";

    assert!(
        write_inlay_image(temp.path(), "stable", &gif, "animated.gif")
            .unwrap_err()
            .contains("unsupported new Inlay image format")
    );
    assert!(
        write_inlay_image(temp.path(), "stable", avif, "source.avif")
            .unwrap_err()
            .contains("unsupported new Inlay image format")
    );
    assert!(
        write_inlay_image(temp.path(), "stable", b"not an image", "broken.png")
            .unwrap_err()
            .contains("unsupported Inlay image format")
    );

    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), prior_metadata);
    assert_eq!(
        serde_json::from_slice::<InlayImageMetadata>(&prior_metadata).unwrap(),
        original_metadata
    );
}

#[test]
fn overwrites_payload_and_metadata_as_one_recoverable_pair() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "replace",
        &encoded_fixture(ImageFormat::Png, 2, 7),
        "first.png",
    )
    .unwrap();

    let second = write_inlay_image(
        temp.path(),
        "replace",
        &encoded_fixture(ImageFormat::Jpeg, 11, 3),
        "second.jpg",
    )
    .unwrap();
    let payload = fs::read(
        temp.path()
            .join("blobstore/inlays")
            .join(format!("{}.bin", hex("replace"))),
    )
    .unwrap();
    let decoded = image::load_from_memory_with_format(&payload, ImageFormat::WebP).unwrap();

    assert_eq!(second.name, "second.jpg");
    assert_eq!((second.width, second.height), (11, 3));
    assert_eq!((decoded.width(), decoded.height()), (11, 3));
    assert!(!temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{}.bin.replace-previous", hex("replace")))
        .exists());
}

#[test]
fn startup_recovery_restores_the_prior_pair_after_interrupted_promotion() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "crash",
        &encoded_fixture(ImageFormat::Png, 4, 6),
        "prior.png",
    )
    .unwrap();
    let encoded_id = hex("crash");
    let payload_path = temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{encoded_id}.bin"));
    let metadata_path = temp
        .path()
        .join("blobstore/metadata")
        .join(format!("{encoded_id}.json"));
    let prior_payload = fs::read(&payload_path).unwrap();
    let prior_metadata = fs::read(&metadata_path).unwrap();
    fs::rename(
        &payload_path,
        payload_path.with_extension("bin.replace-previous"),
    )
    .unwrap();
    fs::rename(
        &metadata_path,
        metadata_path.with_extension("json.replace-previous"),
    )
    .unwrap();
    fs::write(&payload_path, b"interrupted-new-payload").unwrap();
    let transaction_dir = temp.path().join("blobstore/inlay-transactions");
    fs::write(
        transaction_dir.join(format!("{encoded_id}.json")),
        serde_json::to_vec(&json!({
            "id": "crash",
            "suffix": "simulated",
            "hadPayload": true,
            "hadMetadata": true,
        }))
        .unwrap(),
    )
    .unwrap();

    recover_inlay_writes(temp.path()).unwrap();

    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), prior_metadata);
    assert_eq!(fs::read_dir(transaction_dir).unwrap().count(), 0);
}
