use super::{
    decode_physical_key, encode_inlay_image, recover_inlay_writes, respond, sha256_hex,
    write_inlay_image, write_inlay_image_with_suffix, InlayEncodeFormat, InlayEncodeOptions,
    InlayImageMetadata,
};
use image::{imageops::FilterType, DynamicImage, ImageFormat, Rgba, RgbaImage};
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

fn cas_request(method: Method, physical_key: &str, mime: &str, size: u64) -> Request<Vec<u8>> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("mime", mime)
        .append_pair("size", &size.to_string())
        .finish();
    Request::builder()
        .method(method)
        .uri(format!(
            "http://risuasset.localhost/{}?{}",
            hex(physical_key),
            query
        ))
        .body(Vec::new())
        .unwrap()
}

fn write_cas_object(root: &Path, bytes: &[u8]) -> (String, String) {
    let hash = sha256_hex(bytes);
    let physical_key = format!("assets-v2/objects/{}/{}", &hash[..2], &hash[2..]);
    let payload = root.join(physical_key.replace('/', std::path::MAIN_SEPARATOR_STR));
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(payload, bytes).unwrap();
    (hash, physical_key)
}

pub(super) fn write_blob(root: &Path, logical_key: &str, bytes: &[u8], mime: &str) {
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
fn maps_only_exact_lowercase_cas_object_paths() {
    let hash = "ab".repeat(32);
    let physical_key = format!("assets-v2/objects/ab/{}", &hash[2..]);
    assert_eq!(
        decode_physical_key(&format!(
            "http://risuasset.localhost/{}?mime=image%2Fpng&size=1",
            hex(&physical_key)
        )),
        Some(physical_key)
    );

    for invalid in [
        format!("assets-v2/objects/a/{}", &hash[2..]),
        format!("assets-v2/objects/AB/{}", &hash[2..]),
        format!("assets-v2/objects/ab/{}", &hash[3..]),
        format!("assets-v2/objects/ab/{}/extra", &hash[2..]),
    ] {
        assert_eq!(
            decode_physical_key(&format!("http://risuasset.localhost/{}", hex(&invalid))),
            None,
            "accepted {invalid}"
        );
    }
}

#[test]
fn serves_cas_get_head_and_range_with_alias_descriptor_and_strong_hash_etag() {
    let temp = TempDir::new().unwrap();
    let bytes = b"0123456789";
    let (hash, physical_key) = write_cas_object(temp.path(), bytes);

    let get = respond(
        temp.path(),
        cas_request(
            Method::GET,
            &physical_key,
            "application/x-exact",
            bytes.len() as u64,
        ),
    );
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.body(), bytes);
    assert_eq!(get.headers()[header::CONTENT_TYPE], "application/x-exact");
    assert_eq!(
        get.headers()[header::ETAG].to_str().unwrap(),
        format!("\"{hash}\"")
    );

    let head = respond(
        temp.path(),
        cas_request(
            Method::HEAD,
            &physical_key,
            "application/x-exact",
            bytes.len() as u64,
        ),
    );
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.body().is_empty());
    assert_eq!(head.headers()[header::CONTENT_LENGTH], "10");
    assert_eq!(
        head.headers()[header::ETAG].to_str().unwrap(),
        format!("\"{hash}\"")
    );

    let mut ranged = cas_request(
        Method::GET,
        &physical_key,
        "application/x-exact",
        bytes.len() as u64,
    );
    ranged
        .headers_mut()
        .insert(header::RANGE, "bytes=3-6".parse().unwrap());
    let ranged = respond(temp.path(), ranged);
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.body(), b"3456");
    assert_eq!(ranged.headers()[header::CONTENT_RANGE], "bytes 3-6/10");

    let mut conditional = cas_request(
        Method::GET,
        &physical_key,
        "application/x-exact",
        bytes.len() as u64,
    );
    conditional.headers_mut().insert(
        header::IF_NONE_MATCH,
        format!("\"{hash}\"").parse().unwrap(),
    );
    assert_eq!(
        respond(temp.path(), conditional).status(),
        StatusCode::NOT_MODIFIED
    );
}

#[test]
fn rejects_missing_or_corrupt_cas_descriptors_and_preserves_zero_byte_behavior() {
    let temp = TempDir::new().unwrap();
    let (_, physical_key) = write_cas_object(temp.path(), b"abc");

    let missing_hash = "cd".repeat(32);
    let missing_key = format!("assets-v2/objects/cd/{}", &missing_hash[2..]);
    assert_eq!(
        respond(
            temp.path(),
            cas_request(Method::GET, &missing_key, "application/octet-stream", 0),
        )
        .status(),
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        respond(temp.path(), request(Method::GET, &physical_key)).status(),
        StatusCode::NOT_FOUND
    );

    let duplicate_mime = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "http://risuasset.localhost/{}?mime=text%2Fplain&mime=text%2Fhtml&size=3",
            hex(&physical_key)
        ))
        .body(Vec::new())
        .unwrap();
    assert_eq!(
        respond(temp.path(), duplicate_mime).status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        respond(
            temp.path(),
            cas_request(Method::GET, &physical_key, "application/octet-stream", 4),
        )
        .status(),
        StatusCode::NOT_FOUND
    );

    let (_, empty_key) = write_cas_object(temp.path(), b"");
    let empty = respond(
        temp.path(),
        cas_request(Method::GET, &empty_key, "application/octet-stream", 0),
    );
    assert_eq!(empty.status(), StatusCode::OK);
    assert!(empty.body().is_empty());
    assert_eq!(empty.headers()[header::CONTENT_LENGTH], "0");
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

fn crc32(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for byte in bytes {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = (value >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(value & 1)));
        }
    }
    !value
}

fn apng_fixture() -> Vec<u8> {
    let png = encoded_fixture(ImageFormat::Png, 2, 1);
    let insert_at = 8 + 12 + u32::from_be_bytes(png[8..12].try_into().unwrap()) as usize;
    let mut chunk = Vec::new();
    chunk.extend_from_slice(&8u32.to_be_bytes());
    chunk.extend_from_slice(b"acTL");
    chunk.extend_from_slice(&1u32.to_be_bytes());
    chunk.extend_from_slice(&0u32.to_be_bytes());
    chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
    let mut apng = Vec::with_capacity(png.len() + chunk.len());
    apng.extend_from_slice(&png[..insert_at]);
    apng.extend_from_slice(&chunk);
    apng.extend_from_slice(&png[insert_at..]);
    apng
}

fn animated_webp_header() -> Vec<u8> {
    let mut bytes = b"RIFF\x16\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
    bytes.extend_from_slice(&[0x02, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
    bytes
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

#[test]
fn configurable_inlay_encoding_preserves_original_and_skipped_webp_bytes() {
    let png = encoded_fixture(ImageFormat::Png, 8, 4);
    let webp = encoded_fixture(ImageFormat::WebP, 8, 4);
    for (source, format, skip, expected_mime, expected_ext, expected_bytes) in [
        (
            &png,
            InlayEncodeFormat::Original,
            false,
            "image/png",
            "png",
            true,
        ),
        (
            &webp,
            InlayEncodeFormat::Original,
            false,
            "image/webp",
            "webp",
            true,
        ),
        (
            &webp,
            InlayEncodeFormat::Webp,
            true,
            "image/webp",
            "webp",
            true,
        ),
    ] {
        let result = encode_inlay_image(
            "matrix",
            source,
            "source",
            Some(InlayEncodeOptions {
                format,
                quality: 12,
                max_dimension: 0,
                skip_reencode: skip,
            }),
        )
        .unwrap();
        assert_eq!(result.metadata.mime, expected_mime);
        assert_eq!(result.metadata.ext, expected_ext);
        assert_eq!(result.data.as_slice() == source.as_slice(), expected_bytes);
    }
    let png_result = encode_inlay_image(
        "png",
        &png,
        "source",
        Some(InlayEncodeOptions {
            format: InlayEncodeFormat::Png,
            quality: 12,
            max_dimension: 0,
            skip_reencode: false,
        }),
    )
    .unwrap();
    assert_eq!(&png_result.data[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(png_result.metadata.mime, "image/png");
}

#[test]
fn configurable_inlay_encoding_resizes_before_webp_encoding() {
    let source = encoded_fixture(ImageFormat::Png, 20, 10);
    let result = encode_inlay_image(
        "resize",
        &source,
        "source",
        Some(InlayEncodeOptions {
            format: InlayEncodeFormat::Webp,
            quality: 70,
            max_dimension: 5,
            skip_reencode: false,
        }),
    )
    .unwrap();
    assert_eq!((result.metadata.width, result.metadata.height), (5, 3));
    assert_eq!(result.metadata.mime, "image/webp");
}

#[test]
fn configured_webp_quality_is_forwarded_instead_of_using_the_default() {
    let source = encoded_fixture(ImageFormat::Png, 32, 24);
    let rgba = image::load_from_memory_with_format(&source, ImageFormat::Png)
        .unwrap()
        .to_rgba8();
    let configured = encode_inlay_image(
        "quality-70",
        &source,
        "source.png",
        Some(InlayEncodeOptions {
            format: InlayEncodeFormat::Webp,
            quality: 70,
            max_dimension: 0,
            skip_reencode: false,
        }),
    )
    .unwrap();
    let defaulted = encode_inlay_image("quality-85", &source, "source.png", None).unwrap();

    for (encoded, quality) in [(&configured.data, 70.0), (&defaulted.data, 85.0)] {
        let expected = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
            .encode(quality)
            .to_vec();
        assert_eq!(encoded.as_slice(), expected.as_slice());
        let decoded = image::load_from_memory_with_format(encoded, ImageFormat::WebP).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 24));
    }
}

#[test]
fn configured_png_resizes_losslessly_with_truthful_dimensions() {
    let source = encoded_fixture(ImageFormat::Jpeg, 20, 10);
    let result = encode_inlay_image(
        "png-resize",
        &source,
        "source.jpg",
        Some(InlayEncodeOptions {
            format: InlayEncodeFormat::Png,
            quality: 1,
            max_dimension: 5,
            skip_reencode: true,
        }),
    )
    .unwrap();

    assert_eq!(&result.data[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!((result.metadata.width, result.metadata.height), (5, 3));
    let decoded = image::load_from_memory_with_format(&result.data, ImageFormat::Png)
        .unwrap()
        .to_rgba8();
    assert_eq!((decoded.width(), decoded.height()), (5, 3));
    let expected = image::load_from_memory_with_format(&source, ImageFormat::Jpeg)
        .unwrap()
        .resize(5, 3, FilterType::Lanczos3)
        .to_rgba8();
    assert_eq!(decoded, expected);
}

#[test]
fn rejects_animated_webp_before_any_reencode_or_preservation() {
    for format in [
        InlayEncodeFormat::Original,
        InlayEncodeFormat::Webp,
        InlayEncodeFormat::Png,
    ] {
        for skip_reencode in [false, true] {
            let error = match encode_inlay_image(
                "animated",
                &animated_webp_header(),
                "animated.webp",
                Some(InlayEncodeOptions {
                    format: format.clone(),
                    quality: 85,
                    max_dimension: 0,
                    skip_reencode,
                }),
            ) {
                Ok(_) => panic!("animated WebP unexpectedly accepted"),
                Err(error) => error,
            };

            assert!(error.contains("animated WebP"), "unexpected error: {error}");
        }
    }
}

#[test]
fn native_options_clamp_max_dimension_before_original_mode_ignores_it() {
    for (input, expected) in [
        (json!(-1), 0),
        (json!(12.6), 13),
        (json!(4_294_967_296_u64), u32::MAX),
        (json!(u64::MAX), u32::MAX),
    ] {
        let options: InlayEncodeOptions = serde_json::from_value(json!({
            "format": "original",
            "quality": 85,
            "maxDimension": input,
            "skipReencode": false,
        }))
        .unwrap();
        assert_eq!(options.max_dimension, expected);

        let source = encoded_fixture(ImageFormat::Png, 8, 4);
        let result = encode_inlay_image("original", &source, "source.png", Some(options)).unwrap();
        assert_eq!(result.data, source);
    }
}

#[test]
fn native_options_clamp_webp_quality_to_the_encoder_range() {
    for (input, expected) in [(0, 1), (1, 1), (70, 70), (100, 100), (101, 100), (255, 100)] {
        let options: InlayEncodeOptions = serde_json::from_value(json!({
            "format": "webp",
            "quality": input,
            "maxDimension": 0,
            "skipReencode": false,
        }))
        .unwrap();

        assert_eq!(options.quality, expected);
    }
}

#[test]
fn configured_png_converts_jpeg_and_webp_sources() {
    for (name, source) in [
        ("jpeg", encoded_fixture(ImageFormat::Jpeg, 9, 4)),
        ("webp", encoded_fixture(ImageFormat::WebP, 5, 8)),
    ] {
        let result = encode_inlay_image(
            name,
            &source,
            "source",
            Some(InlayEncodeOptions {
                format: InlayEncodeFormat::Png,
                quality: 1,
                max_dimension: 0,
                skip_reencode: false,
            }),
        )
        .unwrap();
        assert_eq!(&result.data[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(result.metadata.mime, "image/png");
        assert_eq!(result.metadata.ext, "png");
    }
}

#[test]
fn skipped_webp_reencodes_when_max_dimension_requires_resize() {
    let source = encoded_fixture(ImageFormat::WebP, 20, 10);
    let result = encode_inlay_image(
        "resized-skip",
        &source,
        "source.webp",
        Some(InlayEncodeOptions {
            format: InlayEncodeFormat::Webp,
            quality: 70,
            max_dimension: 5,
            skip_reencode: true,
        }),
    )
    .unwrap();
    assert_ne!(result.data, source);
    assert_eq!((result.metadata.width, result.metadata.height), (5, 3));
    assert_eq!(result.metadata.mime, "image/webp");
}

#[test]
fn original_inlay_ignores_max_dimension_and_preserves_oriented_jpeg_bytes() {
    let source = with_exif_orientation(encoded_fixture(ImageFormat::Jpeg, 8, 3), 6);
    let result = encode_inlay_image(
        "original-jpeg",
        &source,
        "source.jpg",
        Some(InlayEncodeOptions {
            format: InlayEncodeFormat::Original,
            quality: 1,
            max_dimension: 1,
            skip_reencode: false,
        }),
    )
    .unwrap();
    assert_eq!(result.data, source);
    assert_eq!((result.metadata.width, result.metadata.height), (3, 8));
    assert_eq!(result.metadata.mime, "image/jpeg");
    assert_eq!(result.metadata.ext, "jpg");
}

#[test]
fn first_write_creates_a_missing_app_data_directory() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("new-app-data");

    let metadata = write_inlay_image(
        &root,
        "first-image",
        &encoded_fixture(ImageFormat::Png, 2, 4),
        "first.png",
    )
    .unwrap();

    assert_eq!((metadata.width, metadata.height), (2, 4));
    assert!(root.join("blobstore/inlay-transactions").is_dir());
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
fn rejects_apng_without_mutating_prior_data() {
    let temp = TempDir::new().unwrap();
    let original = encoded_fixture(ImageFormat::Png, 3, 2);
    write_inlay_image(temp.path(), "stable-apng", &original, "stable.png").unwrap();
    let payload_path = temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{}.bin", hex("stable-apng")));
    let metadata_path = temp
        .path()
        .join("blobstore/metadata")
        .join(format!("{}.json", hex("stable-apng")));
    let prior_payload = fs::read(&payload_path).unwrap();
    let prior_metadata = fs::read(&metadata_path).unwrap();

    assert!(
        write_inlay_image(temp.path(), "stable-apng", &apng_fixture(), "animated.png")
            .unwrap_err()
            .contains("APNG")
    );
    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), prior_metadata);
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
            "payloadSha256": "unused-for-incomplete-pair",
            "metadataSha256": "unused-for-incomplete-pair",
        }))
        .unwrap(),
    )
    .unwrap();
    recover_inlay_writes(temp.path()).unwrap();

    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), prior_metadata);
    assert_eq!(fs::read_dir(transaction_dir).unwrap().count(), 0);
}

#[test]
fn startup_recovery_rejects_a_same_length_corrupt_new_pair() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "same-length",
        &encoded_fixture(ImageFormat::Png, 4, 4),
        "prior.png",
    )
    .unwrap();
    let encoded_id = hex("same-length");
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
    fs::write(&payload_path, vec![0; prior_payload.len()]).unwrap();
    fs::write(&metadata_path, &prior_metadata).unwrap();
    let transaction_dir = temp.path().join("blobstore/inlay-transactions");
    fs::write(
        transaction_dir.join(format!("{encoded_id}.json")),
        serde_json::to_vec(&json!({
            "id": "same-length",
            "suffix": "simulated",
            "hadPayload": true,
            "hadMetadata": true,
            "payloadSha256": "expected-new-payload-hash",
            "metadataSha256": "expected-new-metadata-hash",
        }))
        .unwrap(),
    )
    .unwrap();
    recover_inlay_writes(temp.path()).unwrap();

    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), prior_metadata);
}

#[test]
fn startup_recovery_keeps_an_intact_pair_and_cleans_partial_journal_and_stage_files() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "partial-journal",
        &encoded_fixture(ImageFormat::Png, 3, 5),
        "prior.png",
    )
    .unwrap();
    let encoded_id = hex("partial-journal");
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
    let transaction_dir = temp.path().join("blobstore/inlay-transactions");
    let journal = transaction_dir.join(format!("{encoded_id}.json"));
    let journal_temp = transaction_dir.join(format!(".{encoded_id}.partial.json.replace-next"));
    let payload_temp = payload_path
        .parent()
        .unwrap()
        .join(format!(".{encoded_id}.partial.bin.replace-next"));
    let metadata_temp = metadata_path
        .parent()
        .unwrap()
        .join(format!(".{encoded_id}.partial.json.replace-next"));
    fs::write(&journal, br#"{"id":"partial"#).unwrap();
    fs::write(&journal_temp, b"partial journal temp").unwrap();
    fs::write(&payload_temp, b"partial payload").unwrap();
    fs::write(&metadata_temp, b"partial metadata").unwrap();

    recover_inlay_writes(temp.path()).unwrap();

    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), prior_metadata);
    assert!(!journal.exists());
    assert!(!journal_temp.exists());
    assert!(!payload_temp.exists());
    assert!(!metadata_temp.exists());
}

#[test]
fn simulated_disk_full_while_staging_restores_the_prior_pair() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "disk-full",
        &encoded_fixture(ImageFormat::Png, 2, 3),
        "prior.png",
    )
    .unwrap();
    let encoded_id = hex("disk-full");
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
    let suffix = "disk-full-stage";
    let blocked_stage = payload_path
        .parent()
        .unwrap()
        .join(format!(".{encoded_id}.{suffix}.bin.replace-next"));
    fs::write(&blocked_stage, b"simulated exhausted write target").unwrap();

    let error = write_inlay_image_with_suffix(
        temp.path(),
        "disk-full",
        &encoded_fixture(ImageFormat::Jpeg, 8, 4),
        "new.jpg",
        None,
        suffix.to_owned(),
    )
    .unwrap_err();

    assert!(error.contains("stage Inlay payload"));
    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), prior_metadata);
    assert!(!blocked_stage.exists());
    assert_eq!(
        fs::read_dir(temp.path().join("blobstore/inlay-transactions"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn process_kill_after_complete_pair_promotion_keeps_the_hash_valid_new_pair() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "crash-complete",
        &encoded_fixture(ImageFormat::Png, 2, 2),
        "prior.png",
    )
    .unwrap();
    write_inlay_image(
        temp.path(),
        "new-fixture",
        &encoded_fixture(ImageFormat::Jpeg, 7, 5),
        "new.jpg",
    )
    .unwrap();
    let encoded_id = hex("crash-complete");
    let payload_path = temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{encoded_id}.bin"));
    let metadata_path = temp
        .path()
        .join("blobstore/metadata")
        .join(format!("{encoded_id}.json"));
    let fixture_payload = fs::read(
        temp.path()
            .join("blobstore/inlays")
            .join(format!("{}.bin", hex("new-fixture"))),
    )
    .unwrap();
    let fixture_metadata_value: serde_json::Value = serde_json::from_slice(
        &fs::read(
            temp.path()
                .join("blobstore/metadata")
                .join(format!("{}.json", hex("new-fixture"))),
        )
        .unwrap(),
    )
    .unwrap();
    let mut new_metadata = fixture_metadata_value;
    new_metadata["key"] = json!("crash-complete");
    let new_metadata = serde_json::to_vec(&new_metadata).unwrap();
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
    fs::write(&payload_path, &fixture_payload).unwrap();
    fs::write(&metadata_path, &new_metadata).unwrap();
    let transaction_dir = temp.path().join("blobstore/inlay-transactions");
    let committed_temp = transaction_dir.join(format!(
        ".{encoded_id}.process-kill.committed.json.replace-next"
    ));
    fs::write(
        transaction_dir.join(format!("{encoded_id}.json")),
        serde_json::to_vec(&json!({
            "id": "crash-complete",
            "suffix": "process-kill",
            "hadPayload": true,
            "hadMetadata": true,
            "payloadSha256": sha256_hex(&fixture_payload),
            "metadataSha256": sha256_hex(&new_metadata),
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&committed_temp, b"interrupted committed marker").unwrap();

    recover_inlay_writes(temp.path()).unwrap();

    assert_eq!(fs::read(payload_path).unwrap(), fixture_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), new_metadata);
    assert!(!committed_temp.exists());
}

#[test]
fn first_write_recovery_removes_journal_less_staging_and_incomplete_final_files() {
    for promoted_payload in [false, true] {
        let temp = TempDir::new().unwrap();
        let id = if promoted_payload {
            "first-promoted"
        } else {
            "first-staged"
        };
        let encoded_id = hex(id);
        let payload_dir = temp.path().join("blobstore/inlays");
        let metadata_dir = temp.path().join("blobstore/metadata");
        fs::create_dir_all(&payload_dir).unwrap();
        fs::create_dir_all(&metadata_dir).unwrap();
        let payload_path = payload_dir.join(format!("{encoded_id}.bin"));
        let next_payload = payload_dir.join(format!(".{encoded_id}.legacy.bin.replace-next"));
        let next_metadata = metadata_dir.join(format!(".{encoded_id}.legacy.json.replace-next"));
        if promoted_payload {
            fs::write(&payload_path, b"incomplete promoted payload").unwrap();
        } else {
            fs::write(&next_payload, b"staged payload").unwrap();
        }
        fs::write(&next_metadata, b"staged metadata").unwrap();

        recover_inlay_writes(temp.path()).unwrap();

        assert!(!payload_path.exists());
        assert!(!next_payload.exists());
        assert!(!next_metadata.exists());
    }
}

#[test]
fn first_write_recovery_removes_unpromoted_directory_entries() {
    let temp = TempDir::new().unwrap();
    let blobstore_stage = temp.path().join(".blobstore.replace-next-dir");
    let blobstore = temp.path().join("blobstore");
    let transaction_stage = blobstore.join(".inlay-transactions.replace-next-dir");
    fs::create_dir(&blobstore_stage).unwrap();
    fs::create_dir(&blobstore).unwrap();
    fs::create_dir(&transaction_stage).unwrap();

    recover_inlay_writes(temp.path()).unwrap();

    assert!(!blobstore_stage.exists());
    assert!(!transaction_stage.exists());
}

#[test]
fn journal_less_previous_files_restore_an_interrupted_overwrite() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "legacy-previous",
        &encoded_fixture(ImageFormat::Png, 4, 3),
        "prior.png",
    )
    .unwrap();
    let encoded_id = hex("legacy-previous");
    let payload_path = temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{encoded_id}.bin"));
    let prior_payload = fs::read(&payload_path).unwrap();
    let previous_payload = payload_path.with_extension("bin.replace-previous");
    fs::rename(&payload_path, &previous_payload).unwrap();

    recover_inlay_writes(temp.path()).unwrap();

    assert_eq!(fs::read(payload_path).unwrap(), prior_payload);
    assert!(!previous_payload.exists());
}

#[test]
fn committed_journal_left_by_cleanup_failure_never_rolls_back_a_later_opaque_write() {
    let temp = TempDir::new().unwrap();
    write_inlay_image(
        temp.path(),
        "committed-stale",
        &encoded_fixture(ImageFormat::Png, 3, 3),
        "prior.png",
    )
    .unwrap();
    let encoded_id = hex("committed-stale");
    let payload_path = temp
        .path()
        .join("blobstore/inlays")
        .join(format!("{encoded_id}.bin"));
    let metadata_path = temp
        .path()
        .join("blobstore/metadata")
        .join(format!("{encoded_id}.json"));
    let committed_payload = fs::read(&payload_path).unwrap();
    let committed_metadata = fs::read(&metadata_path).unwrap();
    let blocked_cleanup = payload_path.with_extension("bin.replace-previous");
    fs::create_dir(&blocked_cleanup).unwrap();
    fs::copy(
        &metadata_path,
        metadata_path.with_extension("json.replace-previous"),
    )
    .unwrap();
    let transaction_dir = temp.path().join("blobstore/inlay-transactions");
    fs::write(
        transaction_dir.join(format!("{encoded_id}.json")),
        serde_json::to_vec(&json!({
            "id": "committed-stale",
            "suffix": "cleanup-failed",
            "phase": "committed",
            "hadPayload": true,
            "hadMetadata": true,
            "payloadSha256": sha256_hex(&committed_payload),
            "metadataSha256": sha256_hex(&committed_metadata),
        }))
        .unwrap(),
    )
    .unwrap();
    let journal = transaction_dir.join(format!("{encoded_id}.json"));

    recover_inlay_writes(temp.path()).unwrap();

    assert!(journal.exists());
    let opaque_payload = b"later opaque restore bytes";
    write_blob(temp.path(), "committed-stale", opaque_payload, "image/png");
    let opaque_metadata = fs::read(&metadata_path).unwrap();

    recover_inlay_writes(temp.path()).unwrap();

    assert_eq!(fs::read(payload_path).unwrap(), opaque_payload);
    assert_eq!(fs::read(metadata_path).unwrap(), opaque_metadata);
    assert!(journal.exists());
    fs::remove_dir(blocked_cleanup).unwrap();
    recover_inlay_writes(temp.path()).unwrap();
    assert_eq!(fs::read_dir(transaction_dir).unwrap().count(), 0);
}
