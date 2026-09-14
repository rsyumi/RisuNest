use crc32fast::hash as crc32;
use risunest_lib::native_file_jobs::charx::{
    inspect_charx_file, write_charx_file, CharXContainerKind, CharXInspection, CharXLimits,
    CharXParseErrorCode, CharXWriteErrorCode,
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

fn promote_classic_zip_to_zip64(mut archive: Vec<u8>) -> Vec<u8> {
    let eocd = archive.len() - 22;
    assert_eq!(&archive[eocd..eocd + 4], b"PK\x05\x06");
    let entries = u16::from_le_bytes(archive[eocd + 10..eocd + 12].try_into().unwrap()) as u64;
    let directory_size =
        u32::from_le_bytes(archive[eocd + 12..eocd + 16].try_into().unwrap()) as u64;
    let directory_offset =
        u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap()) as u64;
    archive.truncate(eocd);

    archive.extend_from_slice(b"PK\x06\x06");
    archive.extend_from_slice(&44_u64.to_le_bytes());
    archive.extend_from_slice(&45_u16.to_le_bytes());
    archive.extend_from_slice(&45_u16.to_le_bytes());
    archive.extend_from_slice(&0_u32.to_le_bytes());
    archive.extend_from_slice(&0_u32.to_le_bytes());
    archive.extend_from_slice(&entries.to_le_bytes());
    archive.extend_from_slice(&entries.to_le_bytes());
    archive.extend_from_slice(&directory_size.to_le_bytes());
    archive.extend_from_slice(&directory_offset.to_le_bytes());

    archive.extend_from_slice(b"PK\x06\x07");
    archive.extend_from_slice(&0_u32.to_le_bytes());
    archive.extend_from_slice(&(eocd as u64).to_le_bytes());
    archive.extend_from_slice(&1_u32.to_le_bytes());

    archive.extend_from_slice(b"PK\x05\x06");
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&u16::MAX.to_le_bytes());
    archive.extend_from_slice(&u16::MAX.to_le_bytes());
    archive.extend_from_slice(&u32::MAX.to_le_bytes());
    archive.extend_from_slice(&u32::MAX.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive
}

fn set_classic_zip_comment(mut archive: Vec<u8>, comment: &[u8]) -> Vec<u8> {
    let eocd = archive.len() - 22;
    assert_eq!(&archive[eocd..eocd + 4], b"PK\x05\x06");
    archive[eocd + 20..eocd + 22]
        .copy_from_slice(&u16::try_from(comment.len()).unwrap().to_le_bytes());
    archive.extend_from_slice(comment);
    archive
}

fn classic_eocd(entry_count: u16, directory_size: u32, directory_offset: u32) -> Vec<u8> {
    let mut footer = Vec::with_capacity(22);
    footer.extend_from_slice(b"PK\x05\x06");
    footer.extend_from_slice(&0_u16.to_le_bytes());
    footer.extend_from_slice(&0_u16.to_le_bytes());
    footer.extend_from_slice(&entry_count.to_le_bytes());
    footer.extend_from_slice(&entry_count.to_le_bytes());
    footer.extend_from_slice(&directory_size.to_le_bytes());
    footer.extend_from_slice(&directory_offset.to_le_bytes());
    footer.extend_from_slice(&0_u16.to_le_bytes());
    footer
}

fn central_entry_offsets(archive: &[u8]) -> Vec<usize> {
    archive
        .windows(4)
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == b"PK\x01\x02").then_some(index))
        .collect()
}

fn local_offset(archive: &[u8], central_offset: usize) -> usize {
    u32::from_le_bytes(
        archive[central_offset + 42..central_offset + 46]
            .try_into()
            .unwrap(),
    ) as usize
}

fn local_extra_offset(archive: &[u8], local_offset: usize) -> usize {
    let name_length = u16::from_le_bytes(
        archive[local_offset + 26..local_offset + 28]
            .try_into()
            .unwrap(),
    ) as usize;
    local_offset + 30 + name_length
}

fn card_json_with_data(data: &str) -> String {
    format!(r#"{{"spec":"chara_card_v3","spec_version":"3.0","data":{data}}}"#)
}

fn stored_zip_with_data_descriptors(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut central_entries = Vec::new();
    for (name, data) in entries {
        let local_offset = u32::try_from(archive.len()).unwrap();
        let crc = crc32(data);
        let size = u32::try_from(data.len()).unwrap();
        let name_bytes = name.as_bytes();
        let name_length = u16::try_from(name_bytes.len()).unwrap();

        archive.extend_from_slice(b"PK\x03\x04");
        archive.extend_from_slice(&20_u16.to_le_bytes());
        archive.extend_from_slice(&0x0008_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u32.to_le_bytes());
        archive.extend_from_slice(&0_u32.to_le_bytes());
        archive.extend_from_slice(&0_u32.to_le_bytes());
        archive.extend_from_slice(&name_length.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(name_bytes);
        archive.extend_from_slice(data);
        archive.extend_from_slice(b"PK\x07\x08");
        archive.extend_from_slice(&crc.to_le_bytes());
        archive.extend_from_slice(&size.to_le_bytes());
        archive.extend_from_slice(&size.to_le_bytes());
        central_entries.push((name_bytes, crc, size, local_offset));
    }

    let directory_offset = u32::try_from(archive.len()).unwrap();
    for (name, crc, size, local_offset) in central_entries {
        archive.extend_from_slice(b"PK\x01\x02");
        archive.extend_from_slice(&20_u16.to_le_bytes());
        archive.extend_from_slice(&20_u16.to_le_bytes());
        archive.extend_from_slice(&0x0008_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&crc.to_le_bytes());
        archive.extend_from_slice(&size.to_le_bytes());
        archive.extend_from_slice(&size.to_le_bytes());
        archive.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(&0_u32.to_le_bytes());
        archive.extend_from_slice(&local_offset.to_le_bytes());
        archive.extend_from_slice(name);
    }
    let directory_size = u32::try_from(archive.len()).unwrap() - directory_offset;
    let count = u16::try_from(entries.len()).unwrap();
    archive.extend_from_slice(&classic_eocd(count, directory_size, directory_offset));
    archive
}

fn aes_method_zip_with_clear_encryption_flag(name: &str, data: &[u8]) -> Vec<u8> {
    let name = name.as_bytes();
    let name_length = u16::try_from(name.len()).unwrap();
    let size = u32::try_from(data.len()).unwrap();
    let crc = crc32(data);
    let mut aes_extra = Vec::new();
    aes_extra.extend_from_slice(&0x9901_u16.to_le_bytes());
    aes_extra.extend_from_slice(&7_u16.to_le_bytes());
    aes_extra.extend_from_slice(&2_u16.to_le_bytes());
    aes_extra.extend_from_slice(&0x4541_u16.to_le_bytes());
    aes_extra.push(1);
    aes_extra.extend_from_slice(&0_u16.to_le_bytes());

    let mut archive = Vec::new();
    archive.extend_from_slice(b"PK\x03\x04");
    archive.extend_from_slice(&51_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&99_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&crc.to_le_bytes());
    archive.extend_from_slice(&size.to_le_bytes());
    archive.extend_from_slice(&size.to_le_bytes());
    archive.extend_from_slice(&name_length.to_le_bytes());
    archive.extend_from_slice(&u16::try_from(aes_extra.len()).unwrap().to_le_bytes());
    archive.extend_from_slice(name);
    archive.extend_from_slice(&aes_extra);
    archive.extend_from_slice(data);

    let directory_offset = u32::try_from(archive.len()).unwrap();
    archive.extend_from_slice(b"PK\x01\x02");
    archive.extend_from_slice(&51_u16.to_le_bytes());
    archive.extend_from_slice(&51_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&99_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&crc.to_le_bytes());
    archive.extend_from_slice(&size.to_le_bytes());
    archive.extend_from_slice(&size.to_le_bytes());
    archive.extend_from_slice(&name_length.to_le_bytes());
    archive.extend_from_slice(&u16::try_from(aes_extra.len()).unwrap().to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&0_u16.to_le_bytes());
    archive.extend_from_slice(&0_u32.to_le_bytes());
    archive.extend_from_slice(&0_u32.to_le_bytes());
    archive.extend_from_slice(name);
    archive.extend_from_slice(&aes_extra);
    let directory_size = u32::try_from(archive.len()).unwrap() - directory_offset;
    archive.extend_from_slice(&classic_eocd(1, directory_size, directory_offset));
    archive
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
) -> Result<(TempDir, CharXInspection), risunest_lib::native_file_jobs::charx::CharXParseError> {
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
    assert_eq!(error.code(), expected, "{source_name}: {error}");
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
    assert_eq!(json.card_asset_types, ["x-risu-asset", "x-risu-asset"]);
    assert_eq!(card.asset_references[1].order, 1);
    assert_eq!(card.asset_references[2].order, 2);
    assert_eq!(
        card.asset_references[1].normalized_name,
        card.asset_references[2].normalized_name,
    );
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
fn native_export_second_import_preserves_card_graph_and_payload_hashes() {
    let bytes = zip_bytes(&valid_entries(), true);
    let (first_directory, first_inspection) =
        parse_card("Golden.CHARX", &bytes, CharXLimits::default()).expect("first native import");
    let CharXInspection::Card(first) = first_inspection else {
        panic!("first import must produce a card")
    };
    let export_root = first_directory.path().join("exports");
    fs::create_dir(&export_root).expect("export root");

    let exported = write_charx_file(&first, &export_root, || false).expect("native export");

    assert!(exported
        .path
        .starts_with(export_root.canonicalize().unwrap()));
    assert_eq!(
        exported.byte_length,
        fs::metadata(&exported.path).unwrap().len()
    );
    assert_eq!(exported.sha256.len(), 64);
    assert_eq!(fs::read_dir(&export_root).unwrap().count(), 1);

    let second_staging = first_directory.path().join("second-import");
    fs::create_dir(&second_staging).expect("second staging root");
    let second_inspection = inspect_charx_file(
        &exported.path,
        "second.CHARX",
        &second_staging,
        CharXLimits::default(),
        || false,
    )
    .expect("second native import");
    let CharXInspection::Card(second) = second_inspection else {
        panic!("second import must produce a card")
    };

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&second.card_json).unwrap(),
        serde_json::from_str::<serde_json::Value>(&first.card_json).unwrap(),
    );
    assert_eq!(second.asset_references, first.asset_references);
    assert_eq!(payload_graph(&second), payload_graph(&first));
}

#[test]
fn native_export_stores_highly_compressible_card_metadata_for_parser_round_trip() {
    let bytes = zip_bytes(&valid_entries(), true);
    let (directory, inspection) =
        parse_card("Compressible.CHARX", &bytes, CharXLimits::default()).expect("native import");
    let CharXInspection::Card(mut card) = inspection else {
        panic!("fixture must produce a card")
    };
    let mut metadata: serde_json::Value = serde_json::from_str(&card.card_json).unwrap();
    metadata["data"]["description"] = serde_json::Value::String("x".repeat(8 * 1024 * 1024));
    card.card_json = serde_json::to_string(&metadata).unwrap();
    let export_root = directory.path().join("compressible-export");
    fs::create_dir(&export_root).unwrap();

    let exported = write_charx_file(&card, &export_root, || false).expect("native export");
    let second_staging = directory.path().join("compressible-import");
    fs::create_dir(&second_staging).unwrap();
    let second = inspect_charx_file(
        &exported.path,
        "second.CHARX",
        &second_staging,
        CharXLimits::default(),
        || false,
    )
    .expect("stored metadata must satisfy the native parser limits");
    let CharXInspection::Card(second) = second else {
        panic!("second import must produce a card")
    };
    assert_eq!(second.card_json.len(), card.card_json.len());
}

#[test]
fn native_export_rejects_changed_staged_payloads_without_leaving_output() {
    let bytes = zip_bytes(&valid_entries(), false);
    let (directory, inspection) =
        parse_card("Golden.CHARX", &bytes, CharXLimits::default()).expect("native import");
    let CharXInspection::Card(card) = inspection else {
        panic!("fixture must produce a card")
    };
    fs::write(&card.payloads[0].staged_path, b"changed after inspection")
        .expect("change staged payload");
    let export_root = directory.path().join("exports");
    fs::create_dir(&export_root).expect("export root");

    let error = write_charx_file(&card, &export_root, || false)
        .expect_err("changed payload must not be exported");

    assert_eq!(error.code(), CharXWriteErrorCode::PayloadHashMismatch);
    assert_eq!(fs::read_dir(export_root).unwrap().count(), 0);
}

#[test]
fn native_export_cancellation_removes_only_its_owned_output() {
    let bytes = zip_bytes(&valid_entries(), false);
    let (directory, inspection) =
        parse_card("Golden.CHARX", &bytes, CharXLimits::default()).expect("native import");
    let CharXInspection::Card(card) = inspection else {
        panic!("fixture must produce a card")
    };
    let export_root = directory.path().join("exports");
    fs::create_dir(&export_root).expect("export root");
    let unrelated = export_root.join("keep.charx");
    fs::write(&unrelated, b"unrelated").expect("unrelated output");
    let checks = AtomicUsize::new(0);

    let error = write_charx_file(&card, &export_root, || {
        checks.fetch_add(1, Ordering::SeqCst) >= 3
    })
    .expect_err("export must observe cancellation");

    assert_eq!(error.code(), CharXWriteErrorCode::Cancelled);
    assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated");
    assert_eq!(fs::read_dir(export_root).unwrap().count(), 1);
    assert!(card
        .payloads
        .iter()
        .all(|payload| payload.staged_path.exists()));
}

fn payload_graph(
    card: &risunest_lib::native_file_jobs::charx::ParsedCharXDescriptor,
) -> Vec<(
    String,
    Option<String>,
    Option<String>,
    String,
    u64,
    String,
    Vec<String>,
)> {
    card.payloads
        .iter()
        .map(|payload| {
            (
                payload.original_name.clone(),
                payload.extension.clone(),
                payload.normalized_extension.clone(),
                payload.mime_type.clone(),
                payload.decoded_size,
                payload.sha256.clone(),
                payload.card_asset_types.clone(),
            )
        })
        .collect()
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
fn valid_non_jpeg_charx_content_wins_over_a_jpeg_filename() {
    let archive = zip_bytes(&valid_entries(), false);
    let (_directory, inspection) = parse_card("misleading.jpeg", &archive, CharXLimits::default())
        .expect("valid CharX bytes must parse independent of filename");

    assert!(matches!(inspection, CharXInspection::Card(_)));
}

#[test]
fn accepts_signed_data_descriptors_emitted_by_streaming_zip_writers() {
    let card = card_json_with_data(
        r#"{"name":"descriptor","extensions":{},"assets":[{"type":"x-risu-asset","uri":"embeded://assets/payload.bin","name":"payload","ext":"bin"}]}"#,
    );
    let archive = stored_zip_with_data_descriptors(&[
        ("assets/payload.bin", b"payload"),
        ("card.json", card.as_bytes()),
    ]);

    let (_directory, inspection) = parse_card("streamed.charx", &archive, CharXLimits::default())
        .expect("parse a streaming-writer CharX with data descriptors");
    let CharXInspection::Card(card) = inspection else {
        panic!("descriptor fixture must parse as CharX")
    };
    assert_eq!(card.payloads.len(), 1);
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
    let card = card_json_with_data(
        r#"{"name":"crc","extensions":{},"assets":[{"type":"x-risu-asset","uri":"embeded://assets/payload.bin","name":"payload","ext":"bin"}]}"#,
    );
    let mut bytes = zip_bytes(
        &[
            ("assets/payload.bin", asset, CompressionMethod::Stored),
            ("card.json", card.as_bytes(), CompressionMethod::Stored),
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
    let source = write_source(&directory, "cancel-directory.charx", &[0_u8; 128]);
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

#[test]
fn jpeg_content_is_the_ordinary_asset_default_independent_of_filename() {
    let plain = b"\xff\xd8\xff\xe0plain jpeg\xff\xd9";
    let (_directory, inspection) = parse_card("misleading.charx", plain, CharXLimits::default())
        .expect("JPEG content must remain an ordinary asset");
    assert!(matches!(inspection, CharXInspection::OrdinaryJpegAsset(_)));

    let invalid_card = zip_bytes(
        &[(
            "card.json",
            br#"{"spec":"chara_card_v3","data":{"name":"missing extensions"}}"#,
            CompressionMethod::Stored,
        )],
        false,
    );
    let mut appended = b"\xff\xd8\xff\xe0jpeg\xff\xd9".to_vec();
    appended.extend_from_slice(&invalid_card);
    let (_directory, inspection) = parse_card("misleading.bin", &appended, CharXLimits::default())
        .expect("invalid appended container must remain an ordinary JPEG");
    assert!(matches!(inspection, CharXInspection::OrdinaryJpegAsset(_)));
}

#[test]
fn uses_zip_0_6_6_last_eocd_selection_for_a_false_signature_in_the_comment() {
    let mut comment = classic_eocd(2_000, 0, 0);
    comment.extend_from_slice(b"ignored trailing comment bytes");
    let archive = set_classic_zip_comment(zip_bytes(&valid_entries(), false), &comment);
    let mut appended = b"\xff\xd8\xff\xe0jpeg\xff\xd9".to_vec();
    appended.extend_from_slice(&archive);

    let (_directory, inspection) =
        parse_card("false-footer.charx", &appended, CharXLimits::default())
            .expect("last false EOCD makes the JPEG container invalid, not a CharX error");

    assert!(matches!(inspection, CharXInspection::OrdinaryJpegAsset(_)));
}

#[test]
fn parses_an_actual_zip64_footer_after_a_large_jpeg_prefix_with_false_signatures() {
    let archive = promote_classic_zip_to_zip64(zip_bytes(&valid_entries(), false));
    let eocd = archive.len() - 22;
    let locator = eocd - 20;
    let record = locator - 56;
    let nominal_record = usize::try_from(u64::from_le_bytes(
        archive[locator + 8..locator + 16].try_into().unwrap(),
    ))
    .unwrap();
    let mut jpeg = vec![0xaa; 256 * 1024];
    jpeg[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    jpeg[32 * 1024..32 * 1024 + 4].copy_from_slice(b"PK\x05\x06");
    jpeg[nominal_record..nominal_record + 56].copy_from_slice(&archive[record..record + 56]);
    let jpeg_length = jpeg.len();
    jpeg[jpeg_length - 2..].copy_from_slice(&[0xff, 0xd9]);
    jpeg.extend_from_slice(&archive);

    let (_directory, inspection) = parse_card("large-prefix.data", &jpeg, CharXLimits::default())
        .expect("parse actual appended ZIP64 through the bounded archive view");
    let CharXInspection::Card(card) = inspection else {
        panic!("valid appended ZIP64 must be a card")
    };
    assert_eq!(card.container_kind, CharXContainerKind::AppendedCharXJpeg);
    assert_eq!(card.archive_offset, jpeg_length as u64);
}

#[test]
fn zip64_locator_is_authoritative_and_invalid_offsets_do_not_escape_the_archive_view() {
    let mut authoritative = promote_classic_zip_to_zip64(zip_bytes(&valid_entries(), false));
    let eocd = authoritative.len() - 22;
    let locator = eocd - 20;
    let record = locator - 56;
    authoritative[eocd + 8..eocd + 10].copy_from_slice(&1_u16.to_le_bytes());
    authoritative[eocd + 10..eocd + 12].copy_from_slice(&1_u16.to_le_bytes());
    authoritative[record + 24..record + 32].copy_from_slice(&2_000_u64.to_le_bytes());
    authoritative[record + 32..record + 40].copy_from_slice(&2_000_u64.to_le_bytes());
    expect_error(
        "authoritative.charx",
        &authoritative,
        CharXLimits {
            max_entries: 100,
            ..CharXLimits::default()
        },
        CharXParseErrorCode::TooManyEntries,
    );

    let mut corrupt = promote_classic_zip_to_zip64(zip_bytes(&valid_entries(), false));
    let locator = corrupt.len() - 22 - 20;
    let offset = u64::from_le_bytes(corrupt[locator + 8..locator + 16].try_into().unwrap());
    corrupt[locator + 8..locator + 16].copy_from_slice(&(offset + 1).to_le_bytes());
    expect_error(
        "corrupt-locator.charx",
        &corrupt,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    let mut jpeg = b"\xff\xd8\xff\xe0jpeg\xff\xd9".to_vec();
    jpeg.extend_from_slice(&corrupt);
    let (_directory, inspection) =
        parse_card("corrupt-locator.jpeg", &jpeg, CharXLimits::default())
            .expect("invalid appended ZIP64 must remain an ordinary JPEG");
    assert!(matches!(inspection, CharXInspection::OrdinaryJpegAsset(_)));
}

#[test]
fn zip64_record_accepts_classic_disk_sentinels_then_proves_single_disk_authoritatively() {
    let mut archive = promote_classic_zip_to_zip64(zip_bytes(&valid_entries(), false));
    let eocd = archive.len() - 22;
    archive[eocd + 4..eocd + 6].copy_from_slice(&u16::MAX.to_le_bytes());
    archive[eocd + 6..eocd + 8].copy_from_slice(&u16::MAX.to_le_bytes());

    let (_directory, inspection) = parse_card(
        "classic-disk-sentinels.charx",
        &archive,
        CharXLimits::default(),
    )
    .expect("the authoritative ZIP64 locator and record prove a single-disk archive");

    assert!(matches!(inspection, CharXInspection::Card(_)));
}

#[test]
fn rejects_classic_and_zip64_multi_disk_fields() {
    let classic = zip_bytes(&valid_entries(), false);
    let eocd = classic.len() - 22;
    for (name, offset) in [("classic-disk", 4), ("classic-central-disk", 6)] {
        let mut mutated = classic.clone();
        mutated[eocd + offset..eocd + offset + 2].copy_from_slice(&1_u16.to_le_bytes());
        expect_error(
            &format!("{name}.charx"),
            &mutated,
            CharXLimits::default(),
            CharXParseErrorCode::InvalidArchive,
        );
    }
    let mut mismatched_count = classic;
    mismatched_count[eocd + 8..eocd + 10].copy_from_slice(&1_u16.to_le_bytes());
    expect_error(
        "classic-disk-count.charx",
        &mismatched_count,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    let zip64 = promote_classic_zip_to_zip64(zip_bytes(&valid_entries(), false));
    let locator = zip64.len() - 22 - 20;
    let record = locator - 56;
    for (name, offset, width) in [
        ("zip64-locator-disk", locator + 4, 4_usize),
        ("zip64-locator-count", locator + 16, 4),
        ("zip64-record-disk", record + 16, 4),
        ("zip64-record-central-disk", record + 20, 4),
        ("zip64-record-disk-count", record + 24, 8),
    ] {
        let mut mutated = zip64.clone();
        if width == 4 {
            mutated[offset..offset + 4].copy_from_slice(&2_u32.to_le_bytes());
        } else {
            mutated[offset..offset + 8].copy_from_slice(&2_u64.to_le_bytes());
        }
        expect_error(
            &format!("{name}.charx"),
            &mutated,
            CharXLimits::default(),
            CharXParseErrorCode::InvalidArchive,
        );
    }
}

#[test]
fn rejects_central_disk_symlink_and_local_header_disagreements() {
    let archive = zip_bytes(&valid_entries(), false);
    let central = central_entry_offsets(&archive)[0];
    let local = local_offset(&archive, central);

    let mut wrong_disk = archive.clone();
    wrong_disk[central + 34..central + 36].copy_from_slice(&1_u16.to_le_bytes());
    expect_error(
        "disk.charx",
        &wrong_disk,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    let mut symlink = archive.clone();
    symlink[central + 5] = 3;
    symlink[central + 38..central + 42].copy_from_slice(&(0o120777_u32 << 16).to_le_bytes());
    expect_error(
        "symlink.charx",
        &symlink,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    for (name, offset, mask) in [
        ("encryption", local + 6, 1_u16),
        ("descriptor", local + 6, 1_u16 << 3),
        ("compression-flags", local + 6, 1_u16 << 1),
    ] {
        let mut mismatched = archive.clone();
        let flags = u16::from_le_bytes(mismatched[offset..offset + 2].try_into().unwrap());
        mismatched[offset..offset + 2].copy_from_slice(&(flags ^ mask).to_le_bytes());
        expect_error(
            &format!("{name}.charx"),
            &mismatched,
            CharXLimits::default(),
            CharXParseErrorCode::InvalidArchive,
        );
    }

    let mut wrong_method = archive;
    wrong_method[local + 8..local + 10].copy_from_slice(&8_u16.to_le_bytes());
    expect_error(
        "method.charx",
        &wrong_method,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    let archive = zip_bytes(&valid_entries(), false);
    let central = central_entry_offsets(&archive)[0];
    let local = local_offset(&archive, central);
    for (name, flag) in [("encrypted", 1_u16), ("unsupported-flag", 1_u16 << 6)] {
        let mut mutated = archive.clone();
        mutated[central + 8..central + 10].copy_from_slice(&flag.to_le_bytes());
        mutated[local + 6..local + 8].copy_from_slice(&flag.to_le_bytes());
        expect_error(
            &format!("{name}.charx"),
            &mutated,
            CharXLimits::default(),
            CharXParseErrorCode::InvalidArchive,
        );
    }

    let mut unsupported_method = archive;
    unsupported_method[central + 10..central + 12].copy_from_slice(&99_u16.to_le_bytes());
    unsupported_method[local + 8..local + 10].copy_from_slice(&99_u16.to_le_bytes());
    expect_error(
        "unsupported-method.charx",
        &unsupported_method,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    for flag in [1_u16 << 1, 1_u16 << 2] {
        let archive = zip_bytes(&valid_entries(), false);
        let central = central_entry_offsets(&archive)[0];
        let local = local_offset(&archive, central);
        let mut stored_reserved_flag = archive;
        stored_reserved_flag[central + 8..central + 10].copy_from_slice(&flag.to_le_bytes());
        stored_reserved_flag[local + 6..local + 8].copy_from_slice(&flag.to_le_bytes());
        expect_error(
            "stored-reserved-compression-flag.charx",
            &stored_reserved_flag,
            CharXLimits::default(),
            CharXParseErrorCode::InvalidArchive,
        );
    }
}

#[test]
fn rejects_missing_or_inconsistent_local_zip64_size_extra_values() {
    let archive = zip_bytes(&valid_entries(), true);
    let central = central_entry_offsets(&archive)[0];
    let local = local_offset(&archive, central);
    assert_eq!(
        u32::from_le_bytes(archive[local + 18..local + 22].try_into().unwrap()),
        u32::MAX
    );
    assert_eq!(
        u32::from_le_bytes(archive[local + 22..local + 26].try_into().unwrap()),
        u32::MAX
    );
    let extra = local_extra_offset(&archive, local);
    assert_eq!(&archive[extra..extra + 2], &1_u16.to_le_bytes());

    let mut missing = archive.clone();
    missing[extra..extra + 2].copy_from_slice(&2_u16.to_le_bytes());
    expect_error(
        "missing-local-zip64-extra.charx",
        &missing,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    let mut inconsistent = archive;
    let decoded = u64::from_le_bytes(inconsistent[extra + 4..extra + 12].try_into().unwrap());
    inconsistent[extra + 4..extra + 12].copy_from_slice(&(decoded + 1).to_le_bytes());
    expect_error(
        "inconsistent-local-zip64-extra.charx",
        &inconsistent,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );
}

#[test]
fn rejects_raw_aes_method_without_panicking_when_zip_rewrites_the_effective_method() {
    let card = card_json_with_data(r#"{"name":"aes","extensions":{},"assets":[]}"#);
    let archive = aes_method_zip_with_clear_encryption_flag("card.json", card.as_bytes());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_card("aes.charx", &archive, CharXLimits::default())
    }));

    let parse_result = result.expect("hostile AES metadata must never reach a zip reader panic");
    let error = parse_result.expect_err("raw method 99 must be rejected before decoded access");
    assert_eq!(error.code(), CharXParseErrorCode::InvalidArchive);
}

#[test]
fn rejects_aes_extra_without_panicking_when_raw_methods_and_flags_look_ordinary() {
    let card = card_json_with_data(r#"{"name":"aes-extra","extensions":{},"assets":[]}"#);
    let mut archive = aes_method_zip_with_clear_encryption_flag("card.json", card.as_bytes());
    let central = central_entry_offsets(&archive)[0];
    let local = local_offset(&archive, central);
    archive[central + 10..central + 12].copy_from_slice(&0_u16.to_le_bytes());
    archive[local + 8..local + 10].copy_from_slice(&0_u16.to_le_bytes());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_card("aes-extra.charx", &archive, CharXLimits::default())
    }));

    let parse_result = result.expect("AES extra metadata must never reach a zip reader panic");
    let error = parse_result.expect_err("AES extra metadata must reject before decoded access");
    assert_eq!(error.code(), CharXParseErrorCode::InvalidArchive);
}

#[test]
fn rejects_local_data_extents_that_overlap_or_enter_the_central_directory() {
    let mut archive = zip_bytes(&valid_entries(), false);
    let central = central_entry_offsets(&archive)[0];
    archive[central + 20..central + 24].copy_from_slice(&u32::MAX.to_le_bytes());

    expect_error(
        "overlap.charx",
        &archive,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    let mut overlap = zip_bytes(&valid_entries(), false);
    let central_entries = central_entry_offsets(&overlap);
    let first_local = local_offset(&overlap, central_entries[0]);
    let compressed = u32::from_le_bytes(
        overlap[central_entries[0] + 20..central_entries[0] + 24]
            .try_into()
            .unwrap(),
    ) + 1;
    overlap[central_entries[0] + 20..central_entries[0] + 24]
        .copy_from_slice(&compressed.to_le_bytes());
    overlap[first_local + 18..first_local + 22].copy_from_slice(&compressed.to_le_bytes());
    expect_error(
        "local-overlap.charx",
        &overlap,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );

    let mut extra_intrusion = zip_bytes(&valid_entries(), false);
    let last_central = *central_entry_offsets(&extra_intrusion).last().unwrap();
    let last_local = local_offset(&extra_intrusion, last_central);
    extra_intrusion[last_local + 28..last_local + 30].copy_from_slice(&u16::MAX.to_le_bytes());
    expect_error(
        "local-extra-intrusion.charx",
        &extra_intrusion,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidArchive,
    );
}

#[test]
fn validates_card_schema_and_every_asset_before_staging_payloads() {
    for malformed_data in [
        r#"{"name":"no extensions","assets":[]}"#,
        r#"{"name":"bad risuai","extensions":{"risuai":7},"assets":[]}"#,
        r#"{"name":"bad assets","extensions":{"risuai":{}},"assets":{}}"#,
        r#"{"name":"bad asset","extensions":{"risuai":{}},"assets":[null]}"#,
        r#"{"name":"bad uri","extensions":{"risuai":{}},"assets":[{"type":"icon","uri":7,"name":"x","ext":"png"}]}"#,
        r#"{"name":"bad scheme","extensions":{"risuai":{}},"assets":[{"type":"icon","uri":"not a URI","name":"x","ext":"png"}]}"#,
        r#"{"name":"bad type","extensions":{"risuai":{}},"assets":[{"type":7,"uri":"ccdefault:","name":"x","ext":"png"}]}"#,
        r#"{"name":"bad name","extensions":{"risuai":{}},"assets":[{"type":"icon","uri":"ccdefault:","name":7,"ext":"png"}]}"#,
        r#"{"name":"bad ext","extensions":{"risuai":{}},"assets":[{"type":"icon","uri":"ccdefault:","name":"x","ext":7}]}"#,
        r#"{"name":"bad default","extensions":{},"assets":[{"type":"icon","uri":"ccdefault:extra","name":"x","ext":"png"}]}"#,
        r#"{"name":"bad embedded","extensions":{},"assets":[{"type":"icon","uri":"embeded://../escape","name":"x","ext":"png"}]}"#,
        r#"{"name":"bad data","extensions":{},"assets":[{"type":"icon","uri":"data:image/png;base64,%%%","name":"x","ext":"png"}]}"#,
    ] {
        let card = card_json_with_data(malformed_data);
        let archive = zip_bytes(
            &[
                ("assets/payload.bin", b"payload", CompressionMethod::Stored),
                ("card.json", card.as_bytes(), CompressionMethod::Stored),
            ],
            false,
        );
        expect_error(
            "malformed-card.charx",
            &archive,
            CharXLimits::default(),
            CharXParseErrorCode::InvalidCardMetadata,
        );
    }

    let invalid = card_json_with_data(r#"{"name":"invalid first","assets":[]}"#);
    let asset = b"corrupt-before-card";
    let mut archive = zip_bytes(
        &[
            ("assets/payload.bin", asset, CompressionMethod::Stored),
            ("card.json", invalid.as_bytes(), CompressionMethod::Stored),
        ],
        false,
    );
    let payload = archive
        .windows(asset.len())
        .position(|candidate| candidate == asset)
        .unwrap();
    archive[payload] ^= 0xff;
    expect_error(
        "metadata-first.charx",
        &archive,
        CharXLimits::default(),
        CharXParseErrorCode::InvalidCardMetadata,
    );
}

#[test]
fn accepts_mapper_compatible_optional_and_external_asset_uris_without_fetching_them() {
    for data in [
        r#"{"name":"no assets","extensions":{}}"#,
        r#"{"name":"default","extensions":{},"assets":[{"type":"icon","uri":"ccdefault:","name":"default","ext":"png"}]}"#,
        r#"{"name":"data","extensions":{"risuai":{}},"assets":[{"type":"custom","uri":"data:application/octet-stream;base64,cGF5bG9hZA==","name":"data","ext":"bin"}]}"#,
        r#"{"name":"external","extensions":{},"assets":[{"type":"future-type","uri":"https://example.test/asset.png","name":"external","ext":"png"}]}"#,
    ] {
        let card = card_json_with_data(data);
        let archive = zip_bytes(
            &[("card.json", card.as_bytes(), CompressionMethod::Stored)],
            false,
        );
        let (_directory, inspection) =
            parse_card("compatible.charx", &archive, CharXLimits::default())
                .expect("mapper-compatible asset URI must validate without network access");
        assert!(matches!(inspection, CharXInspection::Card(_)));
    }
}

#[test]
fn cancellation_is_checked_after_validation_before_staging_is_preserved() {
    let archive = zip_bytes(
        &[
            ("card.json", CARD_JSON.as_bytes(), CompressionMethod::Stored),
            (
                "assets/Portrait.JPEG",
                b"\xff\xd8\xff\xe0portrait",
                CompressionMethod::Stored,
            ),
            (
                "assets/config.JSON",
                br#"{"enabled":true}"#,
                CompressionMethod::Stored,
            ),
        ],
        false,
    );
    let baseline_checks = AtomicUsize::new(0);
    let (directory, inspection) =
        parse_card("count-checks.charx", &archive, CharXLimits::default()).unwrap();
    let CharXInspection::Card(card) = inspection else {
        panic!("baseline must parse")
    };
    fs::remove_dir_all(card.staging_directory).unwrap();
    drop(directory);

    let directory = TempDir::new().unwrap();
    let staging = directory.path().join("jobs");
    fs::create_dir(&staging).unwrap();
    let source = write_source(&directory, "final-cancel.charx", &archive);
    let first_pass = inspect_charx_file(
        &source,
        "count-checks.charx",
        &staging,
        CharXLimits::default(),
        || {
            baseline_checks.fetch_add(1, Ordering::Relaxed);
            false
        },
    )
    .unwrap();
    let CharXInspection::Card(first_card) = first_pass else {
        panic!("counting pass must parse")
    };
    fs::remove_dir_all(first_card.staging_directory).unwrap();
    let cancel_at = baseline_checks.load(Ordering::Relaxed);
    let checks = AtomicUsize::new(0);

    let error = inspect_charx_file(
        &source,
        "final-cancel.charx",
        &staging,
        CharXLimits::default(),
        || checks.fetch_add(1, Ordering::Relaxed) + 1 >= cancel_at,
    )
    .expect_err("the final cancellation checkpoint must win before preserve");

    assert_eq!(error.code(), CharXParseErrorCode::Cancelled);
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
}

#[test]
fn metadata_has_a_separate_generous_limit_from_binary_assets() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("metadata.charx");
    let card = br#"{"spec":"chara_card_v3","spec_version":"3.0","data":{"name":"synthetic","extensions":{},"assets":[]}}"#;
    fs::write(
        &path,
        zip_bytes(
            &[
                ("card.json", card, CompressionMethod::Stored),
                ("module.risum", &[0_u8; 32], CompressionMethod::Stored),
            ],
            false,
        ),
    )
    .unwrap();
    let limits = CharXLimits {
        max_entry_decoded_bytes: 4,
        max_metadata_bytes: 1024,
        ..CharXLimits::default()
    };
    assert!(matches!(
        inspect_charx_file(
            &path,
            "metadata.charx",
            &directory.path().join("stage"),
            limits,
            || false
        )
        .unwrap(),
        CharXInspection::Card(_)
    ));
    assert_eq!(CharXLimits::default().max_metadata_bytes, 128 * 1024 * 1024);
}
