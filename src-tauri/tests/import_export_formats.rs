use base64::{engine::general_purpose::STANDARD, Engine as _};
use risunest_lib::import_export_jobs::{
    classify_content, parse_json_card, parse_risum, ContentKind, FormatErrorKind, ImportLimits,
    JobStaging,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    rc::Rc,
};
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

const GOLDEN_MODULE_JSON: &[u8] = br#"{"type":"risuModule","module":{"name":"x","assets":[]}}"#;
const GOLDEN_MODULE_RPACK: &[u8] = &[
    230, 32, 15, 108, 44, 5, 32, 151, 32, 121, 14, 72, 178, 219, 64, 226, 178, 76, 5, 32, 0, 32,
    45, 64, 226, 178, 76, 5, 32, 151, 230, 32, 242, 211, 45, 5, 32, 151, 32, 167, 32, 0, 32, 211,
    72, 72, 5, 15, 72, 32, 151, 244, 207, 123, 123,
];

fn limits() -> ImportLimits {
    ImportLimits {
        max_metadata_bytes: 4 * 1024,
        max_payload_bytes: 1024,
        max_aggregate_payload_bytes: 4 * 1024,
        max_payload_count: 8,
        max_container_entries: 16,
        max_container_directory_bytes: 256 * 1024,
        charx_probe_metadata_bytes: 4 * 1024,
    }
}

fn encode_rpack(bytes: &[u8]) -> Vec<u8> {
    let map = include_bytes!("../../src/ts/rpack/rpack_map.bin");
    bytes.iter().map(|byte| map[*byte as usize]).collect()
}

fn risum(main: &Value, assets: &[&[u8]], terminated: bool) -> Vec<u8> {
    let mut result = vec![111, 0];
    let main = encode_rpack(serde_json::to_string(main).unwrap().as_bytes());
    result.extend_from_slice(&(main.len() as u32).to_le_bytes());
    result.extend_from_slice(&main);
    for asset in assets {
        let encoded = encode_rpack(asset);
        result.push(1);
        result.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        result.extend_from_slice(&encoded);
    }
    if terminated {
        result.push(0);
    }
    result
}

fn read_staged(root: &std::path::Path, name: &str) -> Vec<u8> {
    std::fs::read(root.join(name)).unwrap()
}

fn charx(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    for (name, bytes) in entries {
        writer
            .start_file(
                *name,
                FileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn charx_with_many_entries(entry_count: usize) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    for index in 0..entry_count {
        let name = if index == 0 {
            "card.json".to_string()
        } else {
            format!("assets/{index:05}.bin")
        };
        writer
            .start_file(
                name,
                FileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        if index == 0 {
            writer
                .write_all(br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#)
                .unwrap();
        } else {
            writer.write_all(b"x").unwrap();
        }
    }
    writer.finish().unwrap().into_inner()
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
    let comment_length = u16::try_from(comment.len()).unwrap();
    archive[eocd + 20..eocd + 22].copy_from_slice(&comment_length.to_le_bytes());
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

struct OneByteReader {
    bytes: Cursor<Vec<u8>>,
    reads: Rc<Cell<usize>>,
}

impl Read for OneByteReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let read = self.bytes.read(&mut buffer[..1])?;
        self.reads.set(self.reads.get() + usize::from(read > 0));
        Ok(read)
    }
}

struct ChunkedSeekReader {
    bytes: Cursor<Vec<u8>>,
    max_read: usize,
    bytes_read: Rc<Cell<usize>>,
    max_requested: Rc<Cell<usize>>,
}

impl ChunkedSeekReader {
    fn new(bytes: Vec<u8>, max_read: usize) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            max_read,
            bytes_read: Rc::new(Cell::new(0)),
            max_requested: Rc::new(Cell::new(0)),
        }
    }
}

impl Read for ChunkedSeekReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.max_requested
            .set(self.max_requested.get().max(buffer.len()));
        let allowed = buffer.len().min(self.max_read);
        let read = self.bytes.read(&mut buffer[..allowed])?;
        self.bytes_read.set(self.bytes_read.get() + read);
        Ok(read)
    }
}

impl Seek for ChunkedSeekReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.bytes.seek(position)
    }
}

#[test]
fn rpack_decode_matches_the_frozen_javascript_map() {
    assert_eq!(
        risunest_lib::import_export_jobs::decode_rpack(GOLDEN_MODULE_RPACK).unwrap(),
        GOLDEN_MODULE_JSON
    );
}

#[test]
fn risum_stages_positional_assets_and_preserves_declared_extensions() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let main = json!({
        "type": "risuModule",
        "module": {
            "name": "fixture",
            "assets": [
                ["portrait", "", "PNG"],
                ["voice", "", "ogg"]
            ],
            "unknown": {"kept": true}
        }
    });
    let bytes = risum(&main, &[b"image-bytes", b"audio-bytes"], true);

    let parsed = parse_risum(&mut Cursor::new(bytes), &staging, &limits(), &|| false).unwrap();

    assert_eq!(parsed.metadata, main);
    assert_eq!(parsed.assets.len(), 2);
    assert_eq!(parsed.assets[0].position, 0);
    assert_eq!(parsed.assets[0].declared_extension, "PNG");
    assert_eq!(parsed.assets[1].position, 1);
    assert_eq!(parsed.assets[1].declared_extension, "ogg");
    assert_eq!(
        read_staged(root.path(), &parsed.assets[0].payload.staged_name),
        b"image-bytes"
    );
    assert_eq!(
        parsed.assets[0].payload.sha256,
        hex::encode(Sha256::digest(b"image-bytes"))
    );
}

#[test]
fn risum_rejects_bad_framing_limits_and_frame_count_without_leaking_staging_files() {
    let main = json!({
        "type": "risuModule",
        "module": {"name": "fixture", "assets": [["one", "", "png"]]}
    });
    let mut cases = vec![
        ("bad magic", {
            let mut v = risum(&main, &[b"a"], true);
            v[0] = 0;
            v
        }),
        ("bad version", {
            let mut v = risum(&main, &[b"a"], true);
            v[1] = 1;
            v
        }),
        ("missing terminator", risum(&main, &[b"a"], false)),
        ("missing frame", risum(&main, &[], true)),
        ("extra frame", risum(&main, &[b"a", b"b"], true)),
        ("trailing bytes", {
            let mut v = risum(&main, &[b"a"], true);
            v.push(9);
            v
        }),
    ];
    let mut truncated = risum(&main, &[b"asset"], true);
    truncated.pop();
    truncated.pop();
    cases.push(("truncated frame", truncated));

    for (name, bytes) in cases {
        let root = tempfile::tempdir().unwrap();
        let staging = JobStaging::open(root.path()).unwrap();
        let error =
            parse_risum(&mut Cursor::new(bytes), &staging, &limits(), &|| false).expect_err(name);
        assert!(
            matches!(
                error.kind,
                FormatErrorKind::InvalidFormat | FormatErrorKind::LimitExceeded
            ),
            "{name}: {error:?}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0, "{name}");
    }

    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let mut small = limits();
    small.max_payload_bytes = 2;
    let error = parse_risum(
        &mut Cursor::new(risum(&main, &[b"asset"], true)),
        &staging,
        &small,
        &|| false,
    )
    .unwrap_err();
    assert_eq!(error.kind, FormatErrorKind::LimitExceeded);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);

    let mut aggregate = limits();
    aggregate.max_aggregate_payload_bytes = 5;
    let two_assets = json!({
        "type": "risuModule",
        "module": {"assets": [["one", "", "bin"], ["two", "", "bin"]]}
    });
    let error = parse_risum(
        &mut Cursor::new(risum(&two_assets, &[b"abc", b"def"], true)),
        &staging,
        &aggregate,
        &|| false,
    )
    .unwrap_err();
    assert_eq!(error.kind, FormatErrorKind::LimitExceeded);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);

    let mut oversized_main = vec![111, 0];
    oversized_main.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        parse_risum(
            &mut Cursor::new(oversized_main),
            &staging,
            &limits(),
            &|| false,
        )
        .unwrap_err()
        .kind,
        FormatErrorKind::LimitExceeded
    );
}

#[test]
fn risum_cancellation_removes_payloads_created_by_the_parse() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let main = json!({
        "type": "risuModule",
        "module": {"name": "fixture", "assets": [["one", "", "png"], ["two", "", "png"]]}
    });
    let checks = std::cell::Cell::new(0);
    let cancelled = || {
        checks.set(checks.get() + 1);
        checks.get() >= 9
    };

    let error = parse_risum(
        &mut Cursor::new(risum(&main, &[b"first", b"second"], true)),
        &staging,
        &limits(),
        &cancelled,
    )
    .unwrap_err();

    assert_eq!(error.kind, FormatErrorKind::Cancelled);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn risum_checks_cancellation_while_reading_main_metadata() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let main = json!({
        "type": "risuModule",
        "module": {"name": "long enough to require many one-byte reads", "assets": []}
    });
    let bytes = risum(&main, &[], true);
    let reads = Rc::new(Cell::new(0));
    let mut reader = OneByteReader {
        bytes: Cursor::new(bytes.clone()),
        reads: Rc::clone(&reads),
    };
    let cancelled = || reads.get() >= 12;

    let error = parse_risum(&mut reader, &staging, &limits(), &cancelled).unwrap_err();

    assert_eq!(error.kind, FormatErrorKind::Cancelled);
    assert!(
        reads.get() <= 13,
        "read {} bytes after cancellation",
        reads.get()
    );
}

#[test]
fn risum_caps_each_main_metadata_read_at_64_kib() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let main = json!({
        "type": "risuModule",
        "module": {"name": "x".repeat(192 * 1024), "assets": []}
    });
    let bytes = risum(&main, &[], true);
    let mut reader = ChunkedSeekReader::new(bytes, usize::MAX);
    let max_requested = Rc::clone(&reader.max_requested);
    let mut large = limits();
    large.max_metadata_bytes = 256 * 1024;

    parse_risum(&mut reader, &staging, &large, &|| false).unwrap();

    assert!(
        max_requested.get() <= 64 * 1024,
        "requested {} metadata bytes in one read",
        max_requested.get()
    );
}

#[test]
fn json_card_extracts_data_uris_to_bounded_job_staging() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let image = b"png-payload";
    let document = json!({
        "spec": "chara_card_v3",
        "spec_version": "3.0",
        "data": {
            "name": "fixture",
            "unknown": {"kept": true},
            "assets": [{
                "type": "icon",
                "name": "main",
                "ext": "PNG",
                "uri": format!("data:image/png;base64,{}", STANDARD.encode(image))
            }]
        }
    });

    let parsed = parse_json_card(
        &mut Cursor::new(serde_json::to_vec(&document).unwrap()),
        &staging,
        &limits(),
        &|| false,
    )
    .unwrap();

    assert_eq!(parsed.payloads.len(), 1);
    let payload = &parsed.payloads[0];
    assert_eq!(payload.json_pointer, "/data/assets/0/uri");
    assert_eq!(payload.media_type, "image/png");
    assert_eq!(payload.declared_extension.as_deref(), Some("PNG"));
    assert_eq!(payload.extension, "PNG");
    assert_eq!(payload.payload.byte_size, image.len() as u64);
    assert_eq!(payload.payload.sha256, hex::encode(Sha256::digest(image)));
    assert_eq!(
        read_staged(root.path(), &payload.payload.staged_name),
        image
    );
    assert_eq!(
        parsed.metadata.pointer("/data/assets/0/uri").unwrap(),
        &Value::String(format!("__asset:{}", payload.reference_key))
    );
    assert_eq!(
        parsed.metadata.pointer("/data/unknown/kept"),
        Some(&Value::Bool(true))
    );
}

#[test]
fn json_card_derives_a_safe_extension_when_the_card_does_not_declare_one() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let document = json!({
        "spec": "chara_card_v3",
        "data": {"assets": [{
            "uri": format!("data:audio/ogg;base64,{}", STANDARD.encode([1, 2]))
        }]}
    });

    let parsed = parse_json_card(
        &mut Cursor::new(serde_json::to_vec(&document).unwrap()),
        &staging,
        &limits(),
        &|| false,
    )
    .unwrap();

    assert_eq!(parsed.payloads[0].declared_extension, None);
    assert_eq!(parsed.payloads[0].extension, "ogg");
}

#[test]
fn json_card_rejects_invalid_data_uris_and_cleans_partial_payloads() {
    let invalid = [
        "data:image/png,not-base64",
        "data:not-a-media-type;base64,AA==",
        "data:image/png;base64,%%%",
    ];
    for uri in invalid {
        let root = tempfile::tempdir().unwrap();
        let staging = JobStaging::open(root.path()).unwrap();
        let document = json!({
            "spec": "chara_card_v3",
            "data": {"assets": [{"ext": "png", "uri": uri}]}
        });

        let error = parse_json_card(
            &mut Cursor::new(serde_json::to_vec(&document).unwrap()),
            &staging,
            &limits(),
            &|| false,
        )
        .unwrap_err();

        assert_eq!(error.kind, FormatErrorKind::InvalidFormat, "{uri}");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0, "{uri}");
    }
}

#[test]
fn json_card_rejects_a_present_non_string_declared_extension() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let document = json!({
        "spec": "chara_card_v3",
        "data": {"assets": [{
            "ext": 7,
            "uri": format!("data:image/png;base64,{}", STANDARD.encode([1, 2, 3]))
        }]}
    });

    let error = parse_json_card(
        &mut Cursor::new(serde_json::to_vec(&document).unwrap()),
        &staging,
        &limits(),
        &|| false,
    )
    .unwrap_err();

    assert_eq!(error.kind, FormatErrorKind::InvalidFormat);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn json_card_rejects_a_generated_reference_collision() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let document = json!({
        "spec": "chara_card_v3",
        "data": {"assets": [
            {"ext": "png", "uri": format!("data:image/png;base64,{}", STANDARD.encode([1]))},
            {"ext": "png", "uri": "__asset:native-data-0"}
        ]}
    });

    let error = parse_json_card(
        &mut Cursor::new(serde_json::to_vec(&document).unwrap()),
        &staging,
        &limits(),
        &|| false,
    )
    .unwrap_err();

    assert_eq!(error.kind, FormatErrorKind::InvalidFormat);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn json_card_cancellation_removes_payloads_completed_earlier_in_the_parse() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let document = json!({
        "spec": "chara_card_v3",
        "data": {"assets": [
            {"ext": "bin", "uri": format!("data:application/octet-stream;base64,{}", STANDARD.encode([1]))},
            {"ext": "bin", "uri": format!("data:application/octet-stream;base64,{}", STANDARD.encode([2]))}
        ]}
    });
    let checks = Cell::new(0);
    let cancelled = || {
        checks.set(checks.get() + 1);
        checks.get() >= 8
    };

    let error = parse_json_card(
        &mut Cursor::new(serde_json::to_vec(&document).unwrap()),
        &staging,
        &limits(),
        &cancelled,
    )
    .unwrap_err();

    assert_eq!(error.kind, FormatErrorKind::Cancelled);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn json_card_enforces_metadata_payload_and_aggregate_limits() {
    let root = tempfile::tempdir().unwrap();
    let staging = JobStaging::open(root.path()).unwrap();
    let document = json!({
        "spec": "chara_card_v3",
        "data": {"assets": [
            {"ext": "bin", "uri": format!("data:application/octet-stream;base64,{}", STANDARD.encode([1, 2, 3]))},
            {"ext": "bin", "uri": format!("data:application/octet-stream;base64,{}", STANDARD.encode([4, 5, 6]))}
        ]}
    });
    let bytes = serde_json::to_vec(&document).unwrap();

    let mut metadata_limited = limits();
    metadata_limited.max_metadata_bytes = bytes.len() - 1;
    assert_eq!(
        parse_json_card(
            &mut Cursor::new(&bytes),
            &staging,
            &metadata_limited,
            &|| false
        )
        .unwrap_err()
        .kind,
        FormatErrorKind::LimitExceeded
    );

    let mut payload_limited = limits();
    payload_limited.max_payload_bytes = 2;
    assert_eq!(
        parse_json_card(
            &mut Cursor::new(&bytes),
            &staging,
            &payload_limited,
            &|| false
        )
        .unwrap_err()
        .kind,
        FormatErrorKind::LimitExceeded
    );

    let mut aggregate_limited = limits();
    aggregate_limited.max_aggregate_payload_bytes = 5;
    assert_eq!(
        parse_json_card(
            &mut Cursor::new(&bytes),
            &staging,
            &aggregate_limited,
            &|| false
        )
        .unwrap_err()
        .kind,
        FormatErrorKind::LimitExceeded
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn extension_routing_is_case_insensitive_but_content_sniffing_is_authoritative() {
    let card = br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#;
    let ordinary_jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];

    assert_eq!(
        classify_content(
            "MODULE.RISUM",
            &mut Cursor::new(risum(
                &json!({"type":"risuModule","module":{"assets":[]}}),
                &[],
                true
            )),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::RisuModule
    );
    assert_eq!(
        classify_content("CARD.JsOn", &mut Cursor::new(card), &limits(), &|| false).unwrap(),
        ContentKind::JsonCard
    );
    assert_eq!(
        classify_content(
            "misleading.JSON",
            &mut Cursor::new(ordinary_jpeg),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::JpegAsset
    );
    assert_eq!(
        classify_content(
            "misleading.JPEG",
            &mut Cursor::new(card),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::JsonCard
    );
    assert_eq!(
        classify_content(
            "hint-only.RiSuM",
            &mut Cursor::new(b"unknown"),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::RisuModule
    );
    assert_eq!(
        classify_content(
            "hint-only.JsOn",
            &mut Cursor::new(b"unknown"),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::JsonCard
    );
}

#[test]
fn png_signature_classification_is_content_authoritative() {
    let png = b"\x89PNG\r\n\x1a\n";

    assert_eq!(
        classify_content("CARD.PnG", &mut Cursor::new(png), &limits(), &|| false).unwrap(),
        ContentKind::PngCard
    );
    assert_eq!(
        classify_content("misleading.JSON", &mut Cursor::new(png), &limits(), &|| {
            false
        })
        .unwrap(),
        ContentKind::PngCard
    );
    assert_eq!(
        classify_content(
            "not-a-png.png",
            &mut Cursor::new(b"unknown"),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::Unknown
    );
}

#[test]
fn content_sniffing_fills_its_prefix_across_short_reads() {
    let module = risum(
        &json!({"type":"risuModule","module":{"assets":[]}}),
        &[],
        true,
    );
    let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];

    assert_eq!(
        classify_content(
            "misleading.jpeg",
            &mut ChunkedSeekReader::new(module, 1),
            &limits(),
            &|| false,
        )
        .unwrap(),
        ContentKind::RisuModule
    );
    assert_eq!(
        classify_content(
            "misleading.risum",
            &mut ChunkedSeekReader::new(jpeg, 1),
            &limits(),
            &|| false,
        )
        .unwrap(),
        ContentKind::JpegAsset
    );
}

#[test]
fn appended_charx_jpeg_requires_a_bounded_valid_v3_card() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let valid_card = br#"{"spec":"chara_card_v3","spec_version":"3.0","data":{"name":"fixture"}}"#;
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&charx(&[
        ("card.json", valid_card),
        ("asset.png", b"bytes"),
    ]));

    assert_eq!(
        classify_content("card.JpEg", &mut Cursor::new(&appended), &limits(), &|| {
            false
        })
        .unwrap(),
        ContentKind::AppendedCharxJpeg
    );
    assert_eq!(
        classify_content("photo.jpeg", &mut Cursor::new(jpeg), &limits(), &|| false).unwrap(),
        ContentKind::JpegAsset
    );

    let mut missing_card = jpeg.to_vec();
    missing_card.extend_from_slice(&charx(&[("asset.png", b"bytes")]));
    assert_eq!(
        classify_content(
            "photo.jpeg",
            &mut Cursor::new(missing_card),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::JpegAsset
    );

    let mut invalid_card = jpeg.to_vec();
    invalid_card.extend_from_slice(&charx(&[("card.json", b"not-json")]));
    assert_eq!(
        classify_content(
            "photo.jpeg",
            &mut Cursor::new(invalid_card),
            &limits(),
            &|| false
        )
        .unwrap(),
        ContentKind::JpegAsset
    );
}

#[test]
fn appended_charx_probe_accepts_checked_zip64_count_and_offset() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let card = br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#;
    let zip64 = promote_classic_zip_to_zip64(charx(&[("card.json", card)]));
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&zip64);

    assert_eq!(
        classify_content("card.jpeg", &mut Cursor::new(&appended), &limits(), &|| {
            false
        })
        .unwrap(),
        ContentKind::AppendedCharxJpeg
    );

    let locator_offset = appended.len() - 22 - 20 + 8;
    let wrong_offset = u64::from_le_bytes(
        appended[locator_offset..locator_offset + 8]
            .try_into()
            .unwrap(),
    ) + 1;
    appended[locator_offset..locator_offset + 8].copy_from_slice(&wrong_offset.to_le_bytes());
    assert_eq!(
        classify_content("card.jpeg", &mut Cursor::new(appended), &limits(), &|| {
            false
        })
        .unwrap(),
        ContentKind::JpegAsset
    );
}

#[test]
fn charx_preflight_uses_the_same_last_eocd_signature_as_zip_0_6_6() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let card = br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#;
    let mut comment = classic_eocd(2_000, 0, 0);
    comment.extend_from_slice(b"ignored trailing comment bytes");
    let zip = set_classic_zip_comment(charx(&[("card.json", card)]), &comment);
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&zip);
    let expected_preflight_bytes = appended.len() + 20;
    let mut reader = ChunkedSeekReader::new(appended, usize::MAX);
    let bytes_read = Rc::clone(&reader.bytes_read);

    assert_eq!(
        classify_content("card.jpeg", &mut reader, &limits(), &|| false).unwrap(),
        ContentKind::JpegAsset
    );
    assert!(
        bytes_read.get() <= expected_preflight_bytes,
        "read {} bytes after the oversized last EOCD should have been rejected",
        bytes_read.get()
    );
}

#[test]
fn charx_preflight_uses_zip64_count_even_when_classic_count_is_not_a_sentinel() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let card = br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#;
    let mut zip = promote_classic_zip_to_zip64(charx(&[("card.json", card)]));
    let eocd = zip.len() - 22;
    let record = eocd - 20 - 56;
    let directory_size = u64::from_le_bytes(zip[record + 40..record + 48].try_into().unwrap());
    let directory_offset = u64::from_le_bytes(zip[record + 48..record + 56].try_into().unwrap());
    let classic_directory_size = u32::try_from(directory_size + 56 + 20).unwrap();
    zip[eocd + 8..eocd + 10].copy_from_slice(&1_u16.to_le_bytes());
    zip[eocd + 10..eocd + 12].copy_from_slice(&1_u16.to_le_bytes());
    zip[eocd + 12..eocd + 16].copy_from_slice(&classic_directory_size.to_le_bytes());
    zip[eocd + 16..eocd + 20]
        .copy_from_slice(&u32::try_from(directory_offset).unwrap().to_le_bytes());
    zip[record + 24..record + 32].copy_from_slice(&2_000_u64.to_le_bytes());
    zip[record + 32..record + 40].copy_from_slice(&2_000_u64.to_le_bytes());
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&zip);
    let expected_preflight_bytes = appended.len() + 20;
    let mut reader = ChunkedSeekReader::new(appended, usize::MAX);
    let bytes_read = Rc::clone(&reader.bytes_read);

    assert_eq!(
        classify_content("card.jpeg", &mut reader, &limits(), &|| false).unwrap(),
        ContentKind::JpegAsset
    );
    assert!(
        bytes_read.get() <= expected_preflight_bytes,
        "read {} bytes after the oversized ZIP64 count should have been rejected",
        bytes_read.get()
    );
}

#[test]
fn appended_zip64_uses_the_preflight_archive_start_instead_of_a_jpeg_pk0606() {
    let card = br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#;
    let zip = promote_classic_zip_to_zip64(charx(&[("card.json", card)]));
    let eocd = zip.len() - 22;
    let locator = eocd - 20;
    let record = eocd - 20 - 56;
    let nominal_record = usize::try_from(u64::from_le_bytes(
        zip[locator + 8..locator + 16].try_into().unwrap(),
    ))
    .unwrap();
    let mut jpeg = vec![0xaa; 128 * 1024];
    jpeg[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    jpeg[nominal_record..nominal_record + 56].copy_from_slice(&zip[record..record + 56]);
    jpeg.extend_from_slice(&zip);

    assert_eq!(
        classify_content("card.jpeg", &mut Cursor::new(jpeg), &limits(), &|| false).unwrap(),
        ContentKind::AppendedCharxJpeg
    );
}

#[test]
fn appended_zip64_does_not_scan_the_large_jpeg_prefix_past_cancellation() {
    let card = br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#;
    let zip = promote_classic_zip_to_zip64(charx(&[("card.json", card)]));
    let mut jpeg = vec![0xaa; 256 * 1024];
    jpeg[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    jpeg.extend_from_slice(&zip);
    let mut reader = ChunkedSeekReader::new(jpeg, usize::MAX);
    let bytes_read = Rc::clone(&reader.bytes_read);
    let cancelled = || bytes_read.get() >= 70 * 1024;

    assert_eq!(
        classify_content("card.jpeg", &mut reader, &limits(), &cancelled).unwrap(),
        ContentKind::AppendedCharxJpeg
    );
    assert!(bytes_read.get() < 70 * 1024);
}

#[test]
fn appended_charx_probe_rejects_an_archive_beyond_the_entry_limit() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let valid_card = br#"{"spec":"chara_card_v3","data":{"name":"fixture"}}"#;
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&charx(&[
        ("card.json", valid_card),
        ("one.bin", b"1"),
        ("two.bin", b"2"),
        ("three.bin", b"3"),
        ("four.bin", b"4"),
    ]));
    let mut bounded = limits();
    bounded.max_container_entries = 3;

    assert_eq!(
        classify_content("card.jpeg", &mut Cursor::new(appended), &bounded, &|| false).unwrap(),
        ContentKind::JpegAsset
    );
}

#[test]
fn charx_entry_preflight_rejects_before_reading_the_large_central_directory() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&charx_with_many_entries(2_000));
    let mut bounded = limits();
    bounded.max_container_entries = 4;
    let mut reader = ChunkedSeekReader::new(appended, usize::MAX);
    let bytes_read = Rc::clone(&reader.bytes_read);

    assert_eq!(
        classify_content("card.jpeg", &mut reader, &bounded, &|| false).unwrap(),
        ContentKind::JpegAsset
    );
    assert!(
        bytes_read.get() <= 70 * 1024,
        "read {} bytes before rejecting the entry count",
        bytes_read.get()
    );
}

#[test]
fn charx_entry_preflight_checks_cancellation_between_tail_reads() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&charx_with_many_entries(2_000));
    let mut reader = ChunkedSeekReader::new(appended, 64 * 1024);
    let bytes_read = Rc::clone(&reader.bytes_read);
    let cancelled = || bytes_read.get() >= 64 * 1024;

    let error = classify_content("card.jpeg", &mut reader, &limits(), &cancelled).unwrap_err();

    assert_eq!(error.kind, FormatErrorKind::Cancelled);
}

#[test]
fn charx_entry_preflight_caps_central_directory_bytes_before_ziparchive() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9];
    let mut appended = jpeg.to_vec();
    appended.extend_from_slice(&charx_with_many_entries(2_000));
    let mut bounded = limits();
    bounded.max_container_entries = 4_000;
    bounded.max_container_directory_bytes = 1024;
    let mut reader = ChunkedSeekReader::new(appended, usize::MAX);
    let bytes_read = Rc::clone(&reader.bytes_read);

    assert_eq!(
        classify_content("card.jpeg", &mut reader, &bounded, &|| false).unwrap(),
        ContentKind::JpegAsset
    );
    assert!(bytes_read.get() <= 70 * 1024);
}
