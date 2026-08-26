#[path = "../src/owner_manifest_codec.rs"]
mod owner_manifest_codec;

use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use owner_manifest_codec::{
    decode_owner_manifest, decode_owner_manifest_property, encode_owner_manifest,
    encode_owner_manifest_property, owner_manifest_identity, OwnerManifestEntry,
    OwnerManifestProperty,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::hint::black_box;
use std::io::{Read, Write};
use std::time::Instant;

const TUPLES_PER_OWNER: usize = 154_448;
const BENCHMARK_SAMPLES: usize = 3;
const LOCAL_COLD_DECODE_GATE_MS: f64 = 100.0;
const FIXED_PAGE_ENTRIES: usize = 4_096;

fn golden() -> Value {
    serde_json::from_str(include_str!(
        "../../src/ts/storage/tests/fixtures/ownerManifestV1Golden.json"
    ))
    .expect("valid golden fixture")
}

fn golden_entries() -> Vec<OwnerManifestEntry> {
    golden()["entries"]
        .as_array()
        .expect("golden entries")
        .iter()
        .map(|entry| {
            let tuple = entry["tuple"].as_array().expect("golden tuple");
            let payload_hash = entry["payloadHashHex"].as_str().map(|value| {
                hex::decode(value)
                    .expect("golden hash hex")
                    .try_into()
                    .expect("32-byte golden hash")
            });
            OwnerManifestEntry {
                tuple: [
                    tuple[0].as_str().expect("tuple name").to_owned(),
                    tuple[1].as_str().expect("tuple path").to_owned(),
                    tuple[2].as_str().expect("tuple extension").to_owned(),
                ],
                payload_hash,
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
enum FixtureKind {
    Absent,
    Empty,
    ExpectedScale,
    DuplicateHeavy,
    Retained,
}

fn make_benchmark_fixture(
    kind: FixtureKind,
    tuples_per_owner: usize,
) -> Vec<OwnerManifestProperty> {
    match kind {
        FixtureKind::Absent => vec![OwnerManifestProperty::Absent],
        FixtureKind::Empty => vec![OwnerManifestProperty::Present(vec![])],
        FixtureKind::ExpectedScale => vec![OwnerManifestProperty::Present(make_owner_entries(
            "expected-owner",
            tuples_per_owner,
            None,
        ))],
        FixtureKind::DuplicateHeavy => vec![OwnerManifestProperty::Present(make_owner_entries(
            "duplicate-owner",
            tuples_per_owner,
            Some(2),
        ))],
        FixtureKind::Retained => ["module-owner", "persona-owner", "character-owner"]
            .into_iter()
            .map(|owner| {
                OwnerManifestProperty::Present(make_owner_entries(owner, tuples_per_owner, None))
            })
            .collect(),
    }
}

fn make_owner_entries(
    owner: &str,
    entry_count: usize,
    duplicate_slots: Option<usize>,
) -> Vec<OwnerManifestEntry> {
    (0..entry_count)
        .map(|index| {
            let occurrence = duplicate_slots.map_or(index, |slots| index % slots);
            let suffix = format!("{occurrence:06}");
            let path = format!("assets/{owner}/{suffix}.webp");
            let payload_hash = if occurrence % 10 == 0 {
                None
            } else if occurrence % 5 == 0 {
                Some([0x5a; 32])
            } else {
                Some(Sha256::digest(format!("{owner}:{occurrence}")).into())
            };
            OwnerManifestEntry {
                tuple: [format!("{owner}-asset-{suffix}"), path, "webp".to_owned()],
                payload_hash,
            }
        })
        .collect()
}

fn fixture_entry_count(fixture: &[OwnerManifestProperty]) -> usize {
    fixture
        .iter()
        .map(|property| match property {
            OwnerManifestProperty::Absent => 0,
            OwnerManifestProperty::Present(entries) => entries.len(),
        })
        .sum()
}

fn encode_fixture(fixture: &[OwnerManifestProperty]) -> Vec<Option<Vec<u8>>> {
    fixture
        .iter()
        .map(|property| encode_owner_manifest_property(property).expect("encode benchmark fixture"))
        .collect()
}

fn compress_fixture(encoded: &[Option<Vec<u8>>]) -> Vec<Option<Vec<u8>>> {
    encoded
        .iter()
        .map(|manifest| {
            manifest.as_ref().map(|bytes| {
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
                encoder
                    .write_all(bytes)
                    .expect("compress benchmark manifest");
                encoder.finish().expect("finish benchmark compression")
            })
        })
        .collect()
}

fn decode_fixture(encoded: &[Option<Vec<u8>>]) -> usize {
    encoded
        .iter()
        .map(|manifest| match manifest {
            None => {
                assert_eq!(
                    decode_owner_manifest_property(false, None).unwrap(),
                    OwnerManifestProperty::Absent
                );
                0
            }
            Some(bytes) => decode_owner_manifest(bytes)
                .expect("decode benchmark manifest")
                .len(),
        })
        .sum()
}

fn decompress_and_decode_fixture(compressed: &[Option<Vec<u8>>]) -> usize {
    compressed
        .iter()
        .map(|manifest| match manifest {
            None => 0,
            Some(bytes) => {
                let mut decoder = DeflateDecoder::new(bytes.as_slice());
                let mut canonical = Vec::new();
                decoder
                    .read_to_end(&mut canonical)
                    .expect("inflate benchmark manifest");
                decode_owner_manifest(&canonical)
                    .expect("decode inflated benchmark manifest")
                    .len()
            }
        })
        .sum()
}

fn sample_durations(mut operation: impl FnMut()) -> Vec<f64> {
    operation();
    (0..BENCHMARK_SAMPLES)
        .map(|_| {
            let start = Instant::now();
            operation();
            start.elapsed().as_secs_f64() * 1_000.0
        })
        .collect()
}

fn duration_summary(mut samples: Vec<f64>) -> Value {
    samples.sort_by(f64::total_cmp);
    serde_json::json!({
        "medianMs": samples[samples.len() / 2],
        "p95Ms": samples[((samples.len() as f64 * 0.95).ceil() as usize).saturating_sub(1)],
        "samplesMs": samples,
    })
}

fn fixture_identity(encoded: &[Option<Vec<u8>>]) -> String {
    let mut sha256 = Sha256::new();
    for manifest in encoded {
        match manifest {
            None => sha256.update([0]),
            Some(bytes) => {
                sha256.update([1]);
                sha256.update((bytes.len() as u64).to_le_bytes());
                sha256.update(bytes);
            }
        }
    }
    hex::encode(sha256.finalize())
}

fn hash_manifests(encoded: &[Option<Vec<u8>>]) -> Vec<Option<String>> {
    encoded
        .iter()
        .map(|manifest| manifest.as_deref().map(owner_manifest_identity))
        .collect()
}

fn benchmark_fixture(name: &str, fixture: &[OwnerManifestProperty]) -> (Value, f64) {
    let expected_entries = fixture_entry_count(fixture);
    let encoded = encode_fixture(fixture);
    let compressed = compress_fixture(&encoded);
    assert_eq!(decode_fixture(&encoded), expected_entries);
    assert_eq!(decompress_and_decode_fixture(&compressed), expected_entries);

    let encode_samples = sample_durations(|| {
        black_box(encode_fixture(black_box(fixture)));
    });
    let compress_samples = sample_durations(|| {
        black_box(compress_fixture(black_box(&encoded)));
    });
    let raw_hash_samples = sample_durations(|| {
        black_box(hash_manifests(black_box(&encoded)));
    });
    let raw_decode_samples = sample_durations(|| {
        assert_eq!(
            black_box(decode_fixture(black_box(&encoded))),
            expected_entries
        );
    });
    let deflate_decode_samples = sample_durations(|| {
        assert_eq!(
            black_box(decompress_and_decode_fixture(black_box(&compressed))),
            expected_entries
        );
    });
    let deflate_decode = duration_summary(deflate_decode_samples);
    let deflate_decode_p95 = deflate_decode["p95Ms"].as_f64().unwrap();
    let canonical_bytes: usize = encoded.iter().flatten().map(Vec::len).sum();
    let compressed_bytes: usize = compressed.iter().flatten().map(Vec::len).sum();
    let manifest_hashes: Vec<Value> = encoded
        .iter()
        .map(|manifest| match manifest {
            None => Value::Null,
            Some(bytes) => Value::String(owner_manifest_identity(bytes)),
        })
        .collect();

    (
        serde_json::json!({
            "name": name,
            "ownerCount": fixture.len(),
            "entryCount": expected_entries,
            "fixtureHash": fixture_identity(&encoded),
            "manifestHashes": manifest_hashes,
            "canonicalBytes": canonical_bytes,
            "deflateBytes": compressed_bytes,
            "deflateRatio": if canonical_bytes == 0 {
                Value::Null
            } else {
                Value::from(compressed_bytes as f64 / canonical_bytes as f64)
            },
            "encode": duration_summary(encode_samples),
            "deflate": duration_summary(compress_samples),
            "rawHash": duration_summary(raw_hash_samples),
            "rawDecode": duration_summary(raw_decode_samples),
            "deflateDecode": deflate_decode,
        }),
        deflate_decode_p95,
    )
}

fn split_into_fixed_pages(fixture: &[OwnerManifestProperty]) -> Vec<OwnerManifestProperty> {
    fixture
        .iter()
        .flat_map(|property| match property {
            OwnerManifestProperty::Absent => vec![OwnerManifestProperty::Absent],
            OwnerManifestProperty::Present(entries) => entries
                .chunks(FIXED_PAGE_ENTRIES)
                .map(|page| OwnerManifestProperty::Present(page.to_vec()))
                .collect(),
        })
        .collect()
}

#[test]
fn keeps_property_absence_separate_from_canonical_tuple_bytes() {
    assert_eq!(
        encode_owner_manifest_property(&OwnerManifestProperty::Absent).unwrap(),
        None
    );

    let empty_bytes = encode_owner_manifest_property(&OwnerManifestProperty::Present(vec![]))
        .unwrap()
        .expect("present manifest bytes");
    assert_eq!(hex::encode(&empty_bytes), "524f4d460100000000");
    assert_eq!(
        decode_owner_manifest_property(false, None).unwrap(),
        OwnerManifestProperty::Absent
    );
    assert_eq!(
        decode_owner_manifest_property(true, Some(&empty_bytes)).unwrap(),
        OwnerManifestProperty::Present(vec![])
    );
    assert_eq!(
        decode_owner_manifest_property(false, Some(&empty_bytes))
            .unwrap_err()
            .to_string(),
        "absent property cannot have manifest bytes"
    );
    assert_eq!(
        decode_owner_manifest_property(true, None)
            .unwrap_err()
            .to_string(),
        "present property requires manifest bytes"
    );
}

#[test]
fn matches_typescript_golden_bytes_and_identity() {
    let golden = golden();
    let entries = golden_entries();
    let canonical = encode_owner_manifest(&entries).unwrap();

    assert_eq!(
        hex::encode(&canonical),
        golden["canonicalHex"].as_str().unwrap()
    );
    assert_eq!(decode_owner_manifest(&canonical).unwrap(), entries);
    assert_eq!(
        owner_manifest_identity(&canonical),
        golden["manifestHash"].as_str().unwrap()
    );

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&canonical).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut decoder = DeflateDecoder::new(compressed.as_slice());
    let mut inflated = Vec::new();
    decoder.read_to_end(&mut inflated).unwrap();
    assert_eq!(
        owner_manifest_identity(&inflated),
        owner_manifest_identity(&canonical)
    );
}

#[test]
fn rejects_malformed_canonical_bytes() {
    let golden = golden();
    let malformed = [
        "524f4d46010100000001000000ff000000000000000000".to_owned(),
        "524f4d4601010000000200000061".to_owned(),
        "524f4d460102000000".to_owned(),
        format!(
            "524f4d460102000000{}",
            &golden["canonicalHex"].as_str().unwrap()[18..]
        ),
        "524f4d46010100000000000000000000000000000002".to_owned(),
        "524f4d46010000000000".to_owned(),
    ];

    for bytes in malformed {
        assert!(decode_owner_manifest(&hex::decode(bytes).unwrap()).is_err());
    }
}

#[test]
fn benchmark_fixtures_cover_presence_scale_and_duplicates() {
    assert_eq!(
        make_benchmark_fixture(FixtureKind::Absent, 4),
        vec![OwnerManifestProperty::Absent]
    );
    assert_eq!(
        make_benchmark_fixture(FixtureKind::Empty, 4),
        vec![OwnerManifestProperty::Present(vec![])]
    );

    let expected = make_benchmark_fixture(FixtureKind::ExpectedScale, 4);
    assert_eq!(fixture_entry_count(&expected), 4);

    let duplicate_heavy = make_benchmark_fixture(FixtureKind::DuplicateHeavy, 4);
    let OwnerManifestProperty::Present(duplicate_entries) = &duplicate_heavy[0] else {
        panic!("duplicate-heavy owner must be present")
    };
    assert_eq!(duplicate_entries.len(), 4);
    assert_eq!(duplicate_entries[0], duplicate_entries[2]);
    assert_eq!(duplicate_entries[1], duplicate_entries[3]);

    let retained = make_benchmark_fixture(FixtureKind::Retained, 4);
    assert_eq!(retained.len(), 3);
    assert_eq!(fixture_entry_count(&retained), 12);
}

#[test]
#[ignore = "release-only ordered owner-manifest codec benchmark"]
fn owner_manifest_release_benchmark() {
    assert!(
        !cfg!(debug_assertions),
        "owner manifest benchmark requires a release build"
    );
    assert_eq!(
        std::env::var("VITE_DISABLE_REALM").as_deref(),
        Ok("true"),
        "owner manifest benchmark requires VITE_DISABLE_REALM=true"
    );

    let fixtures = [
        ("absent", make_benchmark_fixture(FixtureKind::Absent, 0)),
        ("empty", make_benchmark_fixture(FixtureKind::Empty, 0)),
        (
            "expected-scale",
            make_benchmark_fixture(FixtureKind::ExpectedScale, TUPLES_PER_OWNER),
        ),
        (
            "duplicate-heavy",
            make_benchmark_fixture(FixtureKind::DuplicateHeavy, TUPLES_PER_OWNER),
        ),
        (
            "retained-463344",
            make_benchmark_fixture(FixtureKind::Retained, TUPLES_PER_OWNER),
        ),
    ];

    let mut results = Vec::new();
    let mut expected_scale_p95 = 0.0;
    for (name, fixture) in &fixtures {
        let (result, deflate_decode_p95) = benchmark_fixture(name, fixture);
        if *name == "expected-scale" {
            expected_scale_p95 = deflate_decode_p95;
        }
        results.push(result);
    }

    let fixed_page_comparison = if expected_scale_p95 > LOCAL_COLD_DECODE_GATE_MS {
        let expected = &fixtures[2].1;
        let pages = split_into_fixed_pages(expected);
        let (result, _) = benchmark_fixture("expected-scale-fixed-pages-4096", &pages);
        result
    } else {
        Value::Null
    };

    let output = serde_json::json!({
        "benchmark": "roadmap14-m5-owner-manifest-codec",
        "codecVersion": 1,
        "compression": "raw-deflate-level-6",
        "platform": std::env::consts::OS,
        "profile": "release",
        "sampleCount": BENCHMARK_SAMPLES,
        "tuplesPerExpectedOwner": TUPLES_PER_OWNER,
        "retainedTupleCount": TUPLES_PER_OWNER * 3,
        "localColdDecodeGateMs": LOCAL_COLD_DECODE_GATE_MS,
        "localGatePassed": expected_scale_p95 <= LOCAL_COLD_DECODE_GATE_MS,
        "fixedPageComparison": fixed_page_comparison,
        "fixtures": results,
    });
    println!("owner-manifest-benchmark {output}");
}
