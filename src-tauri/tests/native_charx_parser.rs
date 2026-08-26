use risuai_lib::native_file_jobs::charx::{
    inspect_charx_file, CharXContainerKind, CharXInspection, CharXLimits, CharXParseErrorCode,
};
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

const CARD_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../src/ts/storage/tests/roadmap14/fixtures/charx/card-v3.json"
));

fn zip_bytes(entries: &[(&str, &[u8], CompressionMethod)], zip64: bool) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes, compression) in entries {
        let options = FileOptions::default()
            .compression_method(*compression)
            .large_file(zip64);
        writer
            .start_file(*name, options)
            .expect("start fixture entry");
        writer.write_all(bytes).expect("write fixture entry");
    }
    writer
        .finish()
        .expect("finish fixture archive")
        .into_inner()
}

fn write_source(directory: &TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, bytes).expect("write source fixture");
    path
}

fn parse_card(
    source_name: &str,
    bytes: &[u8],
    limits: CharXLimits,
) -> Result<(TempDir, CharXInspection), risuai_lib::native_file_jobs::charx::CharXParseError> {
    let directory = TempDir::new().expect("source directory");
    let staging = directory.path().join("jobs");
    fs::create_dir(&staging).expect("staging root");
    let source = write_source(&directory, source_name, bytes);
    let result = inspect_charx_file(&source, source_name, &staging, limits, || false)?;
    Ok((directory, result))
}

fn expect_error(
    source_name: &str,
    bytes: &[u8],
    limits: CharXLimits,
    expected: CharXParseErrorCode,
) {
    let directory = TempDir::new().expect("source directory");
    let staging = directory.path().join("jobs");
    fs::create_dir(&staging).expect("staging root");
    let source = write_source(&directory, source_name, bytes);
    let error = inspect_charx_file(&source, source_name, &staging, limits, || false)
        .expect_err("fixture must fail");
    assert_eq!(error.code(), expected);
    assert_eq!(
        fs::read_dir(staging).expect("read staging root").count(),
        0,
        "failed parsing must remove its owned staging directory"
    );
}

fn valid_entries() -> Vec<(&'static str, &'static [u8], CompressionMethod)> {
    vec![
        (
            "assets/Portrait.JPEG",
            b"\xff\xd8\xff\xe0portrait",
            CompressionMethod::Stored,
        ),
        (
            "assets/config.JSON",
            br#"{"enabled":true}"#,
            CompressionMethod::Deflated,
        ),
        (
            "card.json",
            CARD_JSON.as_bytes(),
            CompressionMethod::Deflated,
        ),
    ]
}

#[test]
fn parses_zip64_and_preserves_bounded_card_metadata_and_payload_descriptors() {
    let bytes = zip_bytes(&valid_entries(), true);
    let (_directory, inspection) =
        parse_card("Golden.CHARX", &bytes, CharXLimits::default()).expect("parse ZIP64 CharX");
    let CharXInspection::Card(card) = inspection else {
        panic!("CharX must be classified as a card")
    };

    assert_eq!(card.container_kind, CharXContainerKind::CharX);
    assert_eq!(card.card_json, CARD_JSON);
    assert_eq!(card.archive_offset, 0);
    assert_eq!(card.entry_count, 3);
    assert_eq!(card.payloads.len(), 2);

    let portrait = card
        .payloads
        .iter()
        .find(|payload| payload.original_name == "assets/Portrait.JPEG")
        .expect("portrait descriptor");
    assert_eq!(portrait.extension.as_deref(), Some("JPEG"));
    assert_eq!(portrait.normalized_extension.as_deref(), Some("jpeg"));
    assert_eq!(portrait.mime_type, "image/jpeg");
    assert_eq!(portrait.card_asset_types, ["icon"]);
    assert_eq!(
        fs::read(&portrait.staged_path).expect("staged portrait"),
        b"\xff\xd8\xff\xe0portrait"
    );

    let json = card
        .payloads
        .iter()
        .find(|payload| payload.original_name == "assets/config.JSON")
        .expect("JSON payload descriptor");
    assert_eq!(json.extension.as_deref(), Some("JSON"));
    assert_eq!(json.mime_type, "application/json");
    assert_eq!(json.card_asset_types, ["x-risu-asset"]);
    assert_eq!(
        fs::read(&json.staged_path).expect("staged JSON"),
        br#"{"enabled":true}"#
    );

    assert!(card
        .payloads
        .iter()
        .all(|payload| payload.staged_path.starts_with(&card.staging_directory)));
}

#[test]
fn detects_appended_charx_jpeg_but_keeps_an_ordinary_jpeg_as_an_asset() {
    let archive = zip_bytes(&valid_entries(), false);
    let jpeg_prefix = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00fixture\xff\xd9";
    let mut appended = jpeg_prefix.to_vec();
    appended.extend_from_slice(&archive);

    let (_directory, inspection) = parse_card("appended.JPEG", &appended, CharXLimits::default())
        .expect("parse appended CharX-JPEG");
    let CharXInspection::Card(card) = inspection else {
        panic!("appended container must be classified as a card")
    };
    assert_eq!(card.container_kind, CharXContainerKind::AppendedCharXJpeg);
    assert_eq!(card.archive_offset, jpeg_prefix.len() as u64);

    let ordinary = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00ordinary\xff\xd9";
    let (_directory, inspection) = parse_card("ordinary.jpg", ordinary, CharXLimits::default())
        .expect("classify ordinary JPEG");
    let CharXInspection::OrdinaryJpegAsset(asset) = inspection else {
        panic!("ordinary JPEG must remain an asset")
    };
    assert_eq!(asset.original_name, "ordinary.jpg");
    assert_eq!(asset.extension.as_deref(), Some("jpg"));
    assert_eq!(asset.mime_type, "image/jpeg");
    assert_eq!(asset.byte_length, ordinary.len() as u64);
}

#[test]
fn jpeg_with_a_non_card_trailing_zip_remains_an_asset_before_card_limits_apply() {
    let archive = zip_bytes(
        &[
            ("one.bin", b"one", CompressionMethod::Stored),
            ("two.bin", b"two", CompressionMethod::Stored),
        ],
        false,
    );
    let mut bytes = b"\xff\xd8\xff\xe0ordinary\xff\xd9".to_vec();
    bytes.extend_from_slice(&archive);
    let limits = CharXLimits {
        max_entries: 1,
        ..CharXLimits::default()
    };

    let (_directory, inspection) =
        parse_card("ordinary-with-zip.jpg", &bytes, limits).expect("classify JPEG asset");

    assert!(matches!(inspection, CharXInspection::OrdinaryJpegAsset(_)));
}

#[test]
fn jpeg_with_a_non_card_zip_and_invalid_local_entry_remains_an_asset() {
    let archive = zip_bytes(
        &[("payload.bin", b"payload", CompressionMethod::Stored)],
        false,
    );
    let prefix = b"\xff\xd8\xff\xe0ordinary\xff\xd9";
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(&archive);
    bytes[prefix.len()] ^= 0xff;

    let (_directory, inspection) = parse_card(
        "ordinary-with-invalid-zip.jpeg",
        &bytes,
        CharXLimits::default(),
    )
    .expect("classify JPEG asset without opening non-card entries");

    assert!(matches!(inspection, CharXInspection::OrdinaryJpegAsset(_)));
}

#[test]
fn rejects_unsafe_duplicate_and_sanitized_collision_paths() {
    for path in [
        "/absolute.png",
        "C:/drive.png",
        "assets\\backslash.png",
        "assets/../traversal.png",
        "assets/./dot.png",
        "assets/nul\0.png",
    ] {
        let bytes = zip_bytes(
            &[
                (path, b"asset", CompressionMethod::Stored),
                ("card.json", CARD_JSON.as_bytes(), CompressionMethod::Stored),
            ],
            false,
        );
        expect_error(
            "unsafe.charx",
            &bytes,
            CharXLimits::default(),
            CharXParseErrorCode::InvalidPath,
        );
    }

    let duplicate = zip_bytes(
        &[
            ("assets/same.png", b"one", CompressionMethod::Stored),
            ("assets/same.png", b"two", CompressionMethod::Stored),
            ("card.json", CARD_JSON.as_bytes(), CompressionMethod::Stored),
        ],
        false,
    );
    expect_error(
        "duplicate.charx",
        &duplicate,
        CharXLimits::default(),
        CharXParseErrorCode::DuplicatePath,
    );

    let collision = zip_bytes(
        &[
            ("assets/avatar?.PNG", b"one", CompressionMethod::Stored),
            ("assets/avatar*.png", b"two", CompressionMethod::Stored),
            ("card.json", CARD_JSON.as_bytes(), CompressionMethod::Stored),
        ],
        false,
    );
    expect_error(
        "collision.charx",
        &collision,
        CharXLimits::default(),
        CharXParseErrorCode::SanitizedPathCollision,
    );
}

#[test]
fn enforces_entry_aggregate_ratio_count_and_metadata_limits() {
    let entries = valid_entries();
    let bytes = zip_bytes(&entries, false);

    let limits = CharXLimits {
        max_entries: 2,
        ..CharXLimits::default()
    };
    expect_error(
        "count.charx",
        &bytes,
        limits,
        CharXParseErrorCode::TooManyEntries,
    );

    let limits = CharXLimits {
        max_entry_decoded_bytes: 8,
        ..CharXLimits::default()
    };
    expect_error(
        "entry.charx",
        &bytes,
        limits,
        CharXParseErrorCode::EntryTooLarge,
    );

    let limits = CharXLimits {
        max_total_decoded_bytes: 32,
        ..CharXLimits::default()
    };
    expect_error(
        "aggregate.charx",
        &bytes,
        limits,
        CharXParseErrorCode::AggregateTooLarge,
    );

    let compressible = vec![b'a'; 128 * 1024];
    let ratio_archive = zip_bytes(
        &[
            (
                "assets/compressible.bin",
                &compressible,
                CompressionMethod::Deflated,
            ),
            ("card.json", CARD_JSON.as_bytes(), CompressionMethod::Stored),
        ],
        false,
    );
    let limits = CharXLimits {
        max_compression_ratio: 2,
        ..CharXLimits::default()
    };
    expect_error(
        "ratio.charx",
        &ratio_archive,
        limits,
        CharXParseErrorCode::CompressionRatioExceeded,
    );

    let limits = CharXLimits {
        max_metadata_bytes: 32,
        ..CharXLimits::default()
    };
    expect_error(
        "metadata.charx",
        &bytes,
        limits,
        CharXParseErrorCode::MetadataTooLarge,
    );
}

#[test]
fn verifies_crc_and_removes_owned_staging_on_failure() {
    let asset = b"unique-asset-crc-payload";
    let mut bytes = zip_bytes(
        &[
            ("assets/payload.bin", asset, CompressionMethod::Stored),
            ("card.json", CARD_JSON.as_bytes(), CompressionMethod::Stored),
        ],
        false,
    );
    let offset = bytes
        .windows(asset.len())
        .position(|candidate| candidate == asset)
        .expect("find stored payload");
    bytes[offset] ^= 0xff;

    expect_error(
        "crc.charx",
        &bytes,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidCrc,
    );
}

#[test]
fn cancellation_is_checked_during_payload_copy_and_cleans_partial_files() {
    let large = vec![7_u8; 512 * 1024];
    let bytes = zip_bytes(
        &[
            ("assets/large.bin", &large, CompressionMethod::Stored),
            ("card.json", CARD_JSON.as_bytes(), CompressionMethod::Stored),
        ],
        false,
    );
    let directory = TempDir::new().expect("source directory");
    let staging = directory.path().join("jobs");
    fs::create_dir(&staging).expect("staging root");
    let source = write_source(&directory, "cancel.charx", &bytes);
    let checks = AtomicUsize::new(0);

    let error = inspect_charx_file(
        &source,
        "cancel.charx",
        &staging,
        CharXLimits::default(),
        || checks.fetch_add(1, Ordering::Relaxed) >= 5,
    )
    .expect_err("cancelled parse must fail");

    assert_eq!(error.code(), CharXParseErrorCode::Cancelled);
    assert!(checks.load(Ordering::Relaxed) >= 6);
    assert_eq!(fs::read_dir(staging).expect("read staging root").count(), 0);
}

#[test]
fn cancellation_can_interrupt_archive_directory_parsing_before_format_errors_win() {
    let directory = TempDir::new().expect("source directory");
    let staging = directory.path().join("jobs");
    fs::create_dir(&staging).expect("staging root");
    let source = write_source(&directory, "cancel-directory.charx", b"not a ZIP archive");
    let checks = AtomicUsize::new(0);

    let error = inspect_charx_file(
        &source,
        "cancel-directory.charx",
        &staging,
        CharXLimits::default(),
        || checks.fetch_add(1, Ordering::Relaxed) >= 2,
    )
    .expect_err("directory parsing must observe cancellation");

    assert_eq!(error.code(), CharXParseErrorCode::Cancelled);
    assert!(checks.load(Ordering::Relaxed) >= 3);
    assert_eq!(fs::read_dir(staging).expect("read staging root").count(), 0);
}

#[test]
fn rejects_missing_invalid_or_unreferenced_card_metadata() {
    let missing = zip_bytes(
        &[("assets/payload.bin", b"asset", CompressionMethod::Stored)],
        false,
    );
    expect_error(
        "missing.charx",
        &missing,
        CharXLimits::default(),
        CharXParseErrorCode::MissingCardMetadata,
    );

    let invalid = zip_bytes(
        &[(
            "card.json",
            br#"{"spec":"chara_card_v2"}"#,
            CompressionMethod::Stored,
        )],
        false,
    );
    expect_error(
        "invalid.charx",
        &invalid,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidCardMetadata,
    );

    let missing_asset_card = CARD_JSON.replace("assets/config.JSON", "assets/missing.JSON");
    let unresolved = zip_bytes(
        &[
            (
                "assets/Portrait.JPEG",
                b"\xff\xd8\xff\xe0portrait",
                CompressionMethod::Stored,
            ),
            (
                "card.json",
                missing_asset_card.as_bytes(),
                CompressionMethod::Stored,
            ),
        ],
        false,
    );
    expect_error(
        "unresolved.charx",
        &unresolved,
        CharXLimits::default(),
        CharXParseErrorCode::MissingReferencedAsset,
    );
}

#[test]
fn staging_paths_never_derive_from_archive_names() {
    let bytes = zip_bytes(&valid_entries(), false);
    let (_directory, inspection) =
        parse_card("staging.charx", &bytes, CharXLimits::default()).expect("parse CharX");
    let CharXInspection::Card(card) = inspection else {
        panic!("expected card")
    };
    for payload in card.payloads {
        let file_name = payload
            .staged_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 staging filename");
        assert!(file_name.ends_with(".payload"));
        assert!(!file_name.contains(
            Path::new(&payload.original_name)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        ));
    }
}
