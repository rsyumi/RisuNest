//! Synthetic-only stage benchmark. Never imported by the application.
use risunest_external_storage_format::{content_identity::hash, crypto, pack};
use std::{hint::black_box, io::Write, time::Instant};

fn samples(size: usize, media: bool, mixed: bool) -> Vec<u8> {
    let mut state = 0x7182_9834u32;
    let mut bytes = Vec::new();
    while bytes.len() < size {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        if media {
            bytes.extend_from_slice(&state.to_le_bytes());
        } else if mixed {
            write!(&mut bytes, "{{\"role\":\"assistant\",\"text\":\"").unwrap();
            for index in 0..64 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                write!(
                    &mut bytes,
                    "word{}{}",
                    state % 4096,
                    if index % 8 == 7 { ". " } else { " " }
                )
                .unwrap();
            }
            bytes.extend_from_slice(b"\"}\n");
        } else {
            let index = bytes.len();
            write!(&mut bytes, "{{\"id\":{},\"role\":\"assistant\",\"content\":\"Synthetic message number {}. Local fixture for compression, storage and sync. 합성 대화 데이터.\"}}\n", state, index).unwrap();
        }
    }
    bytes.truncate(size);
    bytes
}
fn measure(name: &str, corpus: &str, input: &[u8], mut operation: impl FnMut() -> usize) {
    operation();
    let mut rates = Vec::new();
    let mut output = 0;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..16 {
            output = black_box(operation());
        }
        rates.push(input.len() as f64 * 16.0 / start.elapsed().as_secs_f64() / 1_000_000.0);
    }
    rates.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({"corpus":corpus,"bytes":input.len(),"stage":name,"medianMBps":rates[2],"ratio":input.len() as f64/output as f64})
    );
}
fn main() {
    println!(
        "{}",
        serde_json::json!({"arch":std::env::consts::ARCH,"os":std::env::consts::OS,"iterations":16,"samples":5,"compressionWorkers":1,"windowLog":pack::MAX_WINDOW_LOG})
    );
    for size in [20 * 1024, pack::MAX_CHUNK_BYTES] {
        for (label, media, mixed) in [
            ("synthetic-json", false, false),
            ("mixed-token-json", false, true),
            ("incompressible-surrogate", true, false),
        ] {
            let input = samples(size, media, mixed);
            for level in [1, 3] {
                let mut context = zstd::zstd_safe::CCtx::create();
                context
                    .set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
                    .unwrap();
                context
                    .set_parameter(zstd::zstd_safe::CParameter::WindowLog(pack::MAX_WINDOW_LOG))
                    .unwrap();
                let mut output = Vec::with_capacity(zstd::zstd_safe::compress_bound(size));
                measure(&format!("zstd-{level}"), label, &input, || {
                    output.clear();
                    context.compress2(&mut output, &input).unwrap()
                });
                assert_eq!(zstd::bulk::decompress(&output, input.len()).unwrap(), input);
                println!(
                    "{}",
                    serde_json::json!({"stage":format!("zstd-{level}"),"corpus":label,"bytes":size,"encoderWorkspaceBytes":context.sizeof()})
                );
            }
            measure("flate2-zlib-6", label, &input, || {
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
                encoder.write_all(&input).unwrap();
                encoder.finish().unwrap().len()
            });
            measure("sha256", label, &input, || {
                black_box(hash(&input));
                input.len()
            });
            measure("dryoc-secretstream", label, &input, || {
                let mut output = Vec::new();
                crypto::encrypt(
                    &mut std::io::Cursor::new(&input),
                    &mut output,
                    &[7; 32],
                    b"synthetic-benchmark",
                    input.len() as u64,
                )
                .unwrap();
                output.len()
            });
            let mut encoder = pack::ChunkEncoder::new().unwrap();
            let policy = if media {
                pack::CompressionPolicy::AlreadyCompressed
            } else {
                pack::CompressionPolicy::Text
            };
            measure("product-compress-encrypt", label, &input, || {
                let encoded = encoder.encode(&input, policy).unwrap();
                let mut output = Vec::new();
                crypto::encrypt(
                    &mut std::io::Cursor::new(&encoded),
                    &mut output,
                    &[7; 32],
                    b"synthetic-benchmark",
                    encoded.len() as u64,
                )
                .unwrap();
                output.len()
            });
        }
    }
}
