use base64::{engine::general_purpose::STANDARD, Engine as _};
use risuai_lib::import_export_jobs::{
    classify_content, parse_json_card, parse_risum, ContentKind, FormatErrorKind, ImportLimits,
    JobStaging,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    io::{Cursor, Read, Write},
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

#[test]
fn rpack_decode_matches_the_frozen_javascript_map() {
    assert_eq!(
        risuai_lib::import_export_jobs::decode_rpack(GOLDEN_MODULE_RPACK).unwrap(),
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
