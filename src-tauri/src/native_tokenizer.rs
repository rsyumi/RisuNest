use serde::{Deserialize, Serialize};
use tiktoken_rs::{cl100k_base_singleton, o200k_base_singleton, CoreBPE};

const CL100K_FINGERPRINT: &str = "cl100k_base:dqbd-1.0.22:49a4e05dea02c8fafbd50cc4725c4aab8f39386c0afedef118e0dfebc2fe523a:tiktoken-rs-0.12.0:contract-1";
const O200K_FINGERPRINT: &str = "o200k_base:dqbd-1.0.22:a2c363f80642c0f07d916716b3940ff030121f3b5cef72b2c5bd1d4f64c14fb8:tiktoken-rs-0.12.0:contract-1";
const MAX_BATCH_ITEMS: usize = 1_000;
const MAX_AGGREGATE_INPUT_BYTES: usize = 1_048_576;
const MAX_RESPONSE_IDS: usize = 262_144;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeTokenizerId {
    Cl100kBase,
    O200kBase,
}

impl NativeTokenizerId {
    fn bpe(self) -> &'static CoreBPE {
        match self {
            Self::Cl100kBase => cl100k_base_singleton(),
            Self::O200kBase => o200k_base_singleton(),
        }
    }

    fn fingerprint(self) -> &'static str {
        match self {
            Self::Cl100kBase => CL100K_FINGERPRINT,
            Self::O200kBase => O200K_FINGERPRINT,
        }
    }

    fn special_tokens(self) -> &'static [&'static str] {
        match self {
            Self::Cl100kBase => &[
                "<|endoftext|>",
                "<|fim_prefix|>",
                "<|fim_middle|>",
                "<|fim_suffix|>",
                "<|endofprompt|>",
            ],
            Self::O200kBase => &["<|endoftext|>", "<|endofprompt|>"],
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeTokenizeMode {
    Count,
    Ids,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TokenizeBatchRequest {
    pub tokenizer_id: NativeTokenizerId,
    pub artifact_fingerprint: String,
    pub mode: NativeTokenizeMode,
    pub texts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TokenizeBatchResponse {
    Count {
        artifact_fingerprint: String,
        counts: Vec<u32>,
    },
    Ids {
        artifact_fingerprint: String,
        ids: Vec<Vec<u32>>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NativeTokenizerError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub special_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<usize>,
}

impl NativeTokenizerError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            index: None,
            special_token: None,
            expected_fingerprint: None,
            received_fingerprint: None,
            limit: None,
            actual: None,
        }
    }
}

fn tokenize_batch_core(
    request: TokenizeBatchRequest,
) -> Result<TokenizeBatchResponse, NativeTokenizerError> {
    let expected_fingerprint = request.tokenizer_id.fingerprint();
    if request.artifact_fingerprint != expected_fingerprint {
        return Err(NativeTokenizerError {
            code: "unsupported_artifact_fingerprint".to_string(),
            message: "The native tokenizer artifact fingerprint is not supported.".to_string(),
            index: None,
            special_token: None,
            expected_fingerprint: Some(expected_fingerprint.to_string()),
            received_fingerprint: Some(request.artifact_fingerprint),
            limit: None,
            actual: None,
        });
    }

    if request.texts.len() > MAX_BATCH_ITEMS {
        return Err(NativeTokenizerError {
            code: "batch_item_limit_exceeded".to_string(),
            message: "Native tokenizer batch item limit exceeded.".to_string(),
            index: None,
            special_token: None,
            expected_fingerprint: None,
            received_fingerprint: None,
            limit: Some(MAX_BATCH_ITEMS),
            actual: Some(request.texts.len()),
        });
    }

    let aggregate_input_bytes = request
        .texts
        .iter()
        .try_fold(0usize, |total, text| total.checked_add(text.len()))
        .unwrap_or(usize::MAX);
    if aggregate_input_bytes > MAX_AGGREGATE_INPUT_BYTES {
        return Err(NativeTokenizerError {
            code: "aggregate_input_limit_exceeded".to_string(),
            message: "Native tokenizer aggregate UTF-8 input limit exceeded.".to_string(),
            index: None,
            special_token: None,
            expected_fingerprint: None,
            received_fingerprint: None,
            limit: Some(MAX_AGGREGATE_INPUT_BYTES),
            actual: Some(aggregate_input_bytes),
        });
    }

    for (index, text) in request.texts.iter().enumerate() {
        let disallowed = request
            .tokenizer_id
            .special_tokens()
            .iter()
            .copied()
            .filter_map(|token| text.find(token).map(|position| (position, token)))
            .min_by_key(|(position, _)| *position)
            .map(|(_, token)| token);
        if let Some(token) = disallowed {
            return Err(NativeTokenizerError {
                code: "disallowed_special_token".to_string(),
                message: format!("The text contains a special token that is not allowed: {token}"),
                index: Some(index),
                special_token: Some(token.to_string()),
                expected_fingerprint: None,
                received_fingerprint: None,
                limit: None,
                actual: None,
            });
        }
    }

    match request.mode {
        NativeTokenizeMode::Ids => {
            let mut ids = Vec::with_capacity(request.texts.len());
            let mut response_ids = 0usize;
            for text in &request.texts {
                let encoded = request.tokenizer_id.bpe().encode_ordinary(text);
                response_ids = response_ids.saturating_add(encoded.len());
                if response_ids > MAX_RESPONSE_IDS {
                    return Err(NativeTokenizerError {
                        code: "response_id_limit_exceeded".to_string(),
                        message: "Native tokenizer ID response limit exceeded.".to_string(),
                        index: None,
                        special_token: None,
                        expected_fingerprint: None,
                        received_fingerprint: None,
                        limit: Some(MAX_RESPONSE_IDS),
                        actual: Some(response_ids),
                    });
                }
                ids.push(
                    encoded
                        .into_iter()
                        .map(|id| {
                            u32::try_from(id).map_err(|_| {
                                NativeTokenizerError::new(
                                    "token_id_overflow",
                                    "token ID does not fit in the native response type",
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            Ok(TokenizeBatchResponse::Ids {
                artifact_fingerprint: request.tokenizer_id.fingerprint().to_string(),
                ids,
            })
        }
        NativeTokenizeMode::Count => {
            let counts = request
                .texts
                .iter()
                .map(|text| {
                    u32::try_from(request.tokenizer_id.bpe().count_ordinary(text)).map_err(|_| {
                        NativeTokenizerError::new(
                            "token_count_overflow",
                            "token count does not fit in a JavaScript-safe response",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(TokenizeBatchResponse::Count {
                artifact_fingerprint: request.tokenizer_id.fingerprint().to_string(),
                counts,
            })
        }
    }
}

#[tauri::command]
pub async fn tokenize_batch(
    request: TokenizeBatchRequest,
) -> Result<TokenizeBatchResponse, NativeTokenizerError> {
    tauri::async_runtime::spawn_blocking(move || tokenize_batch_core(request))
        .await
        .map_err(|error| {
            NativeTokenizerError::new(
                "tokenizer_worker_failed",
                format!("Native tokenizer worker failed: {error}"),
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Corpus {
        cases: Vec<CorpusCase>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CorpusCase {
        name: String,
        tokenizer_id: NativeTokenizerId,
        input: CorpusInput,
        ids: Option<Vec<u32>>,
        error: Option<CorpusError>,
    }

    #[derive(Deserialize)]
    struct CorpusError {
        code: String,
        token: String,
    }

    #[derive(Deserialize)]
    #[serde(
        tag = "kind",
        rename_all = "kebab-case",
        rename_all_fields = "camelCase"
    )]
    enum CorpusInput {
        Text { value: String },
        Utf16CodeUnits { code_units: Vec<u16> },
        Repeat { value: String, count: usize },
    }

    impl CorpusInput {
        fn resolve(&self) -> String {
            match self {
                Self::Text { value } => value.clone(),
                Self::Utf16CodeUnits { code_units } => {
                    char::decode_utf16(code_units.iter().copied())
                        .map(|value| value.unwrap_or(char::REPLACEMENT_CHARACTER))
                        .collect()
                }
                Self::Repeat { value, count } => value.repeat(*count),
            }
        }
    }

    fn fingerprint(tokenizer_id: NativeTokenizerId) -> &'static str {
        tokenizer_id.fingerprint()
    }

    #[test]
    fn cl100k_ascii_ids_match_the_checked_in_oracle() {
        let response = tokenize_batch_core(TokenizeBatchRequest {
            tokenizer_id: NativeTokenizerId::Cl100kBase,
            artifact_fingerprint: CL100K_FINGERPRINT.to_string(),
            mode: NativeTokenizeMode::Ids,
            texts: vec!["hello".to_string()],
        })
        .expect("cl100k request should succeed");

        assert_eq!(
            response,
            TokenizeBatchResponse::Ids {
                artifact_fingerprint: CL100K_FINGERPRINT.to_string(),
                ids: vec![vec![15339]],
            }
        );
    }

    #[test]
    fn successful_corpus_entries_have_exact_native_ids() {
        let corpus: Corpus = serde_json::from_str(include_str!(
            "../../benchmarks/tokenizer/native-tokenizer-corpus.json"
        ))
        .expect("checked-in tokenizer corpus should parse");

        for entry in corpus.cases {
            let Some(expected_ids) = entry.ids else {
                continue;
            };
            let response = tokenize_batch_core(TokenizeBatchRequest {
                tokenizer_id: entry.tokenizer_id,
                artifact_fingerprint: fingerprint(entry.tokenizer_id).to_string(),
                mode: NativeTokenizeMode::Ids,
                texts: vec![entry.input.resolve()],
            })
            .unwrap_or_else(|error| panic!("{} failed: {error:?}", entry.name));
            let TokenizeBatchResponse::Ids { ids, .. } = response else {
                panic!("{} returned the wrong response mode", entry.name);
            };
            assert_eq!(ids, vec![expected_ids], "{} IDs differed", entry.name);
        }
    }

    #[test]
    fn count_mode_preserves_empty_and_duplicate_batch_items() {
        let response = tokenize_batch_core(TokenizeBatchRequest {
            tokenizer_id: NativeTokenizerId::O200kBase,
            artifact_fingerprint: O200K_FINGERPRINT.to_string(),
            mode: NativeTokenizeMode::Count,
            texts: vec![String::new(), "hello".to_string(), "hello".to_string()],
        })
        .expect("count batch should succeed");

        assert_eq!(
            response,
            TokenizeBatchResponse::Count {
                artifact_fingerprint: O200K_FINGERPRINT.to_string(),
                counts: vec![0, 1, 1],
            }
        );
    }

    #[test]
    fn special_token_errors_match_the_oracle_and_report_the_batch_index() {
        let corpus: Corpus = serde_json::from_str(include_str!(
            "../../benchmarks/tokenizer/native-tokenizer-corpus.json"
        ))
        .expect("checked-in tokenizer corpus should parse");

        for entry in corpus.cases {
            let Some(expected_error) = entry.error else {
                continue;
            };
            let error = tokenize_batch_core(TokenizeBatchRequest {
                tokenizer_id: entry.tokenizer_id,
                artifact_fingerprint: fingerprint(entry.tokenizer_id).to_string(),
                mode: NativeTokenizeMode::Ids,
                texts: vec!["safe prefix".to_string(), entry.input.resolve()],
            })
            .expect_err("special token should reject the entire batch");

            assert_eq!(
                serde_json::to_value(error).expect("error should serialize"),
                serde_json::json!({
                    "code": expected_error.code,
                    "message": format!(
                        "The text contains a special token that is not allowed: {}",
                        expected_error.token
                    ),
                    "index": 1,
                    "special_token": expected_error.token,
                }),
                "{} error differed",
                entry.name,
            );
        }
    }

    #[test]
    fn rejects_an_unreviewed_artifact_fingerprint() {
        let error = tokenize_batch_core(TokenizeBatchRequest {
            tokenizer_id: NativeTokenizerId::Cl100kBase,
            artifact_fingerprint: "cl100k_base:unreviewed".to_string(),
            mode: NativeTokenizeMode::Count,
            texts: vec!["hello".to_string()],
        })
        .expect_err("unreviewed artifact should be rejected");

        assert_eq!(
            serde_json::to_value(error).expect("error should serialize"),
            serde_json::json!({
                "code": "unsupported_artifact_fingerprint",
                "message": "The native tokenizer artifact fingerprint is not supported.",
                "expected_fingerprint": CL100K_FINGERPRINT,
                "received_fingerprint": "cl100k_base:unreviewed",
            })
        );
    }

    #[test]
    fn rejects_more_than_one_thousand_batch_items_before_encoding() {
        let error = tokenize_batch_core(TokenizeBatchRequest {
            tokenizer_id: NativeTokenizerId::Cl100kBase,
            artifact_fingerprint: CL100K_FINGERPRINT.to_string(),
            mode: NativeTokenizeMode::Count,
            texts: vec![String::new(); 1_001],
        })
        .expect_err("oversized batch should be rejected");

        assert_eq!(
            serde_json::to_value(error).expect("error should serialize"),
            serde_json::json!({
                "code": "batch_item_limit_exceeded",
                "message": "Native tokenizer batch item limit exceeded.",
                "limit": 1_000,
                "actual": 1_001,
            })
        );
    }

    #[test]
    fn rejects_more_than_one_mebibyte_of_aggregate_utf8_input() {
        let text = "x".repeat(1_048_577);
        let error = tokenize_batch_core(TokenizeBatchRequest {
            tokenizer_id: NativeTokenizerId::O200kBase,
            artifact_fingerprint: O200K_FINGERPRINT.to_string(),
            mode: NativeTokenizeMode::Count,
            texts: vec![text],
        })
        .expect_err("oversized input should be rejected");

        assert_eq!(
            serde_json::to_value(error).expect("error should serialize"),
            serde_json::json!({
                "code": "aggregate_input_limit_exceeded",
                "message": "Native tokenizer aggregate UTF-8 input limit exceeded.",
                "limit": 1_048_576,
                "actual": 1_048_577,
            })
        );
    }

    #[test]
    fn rejects_an_ids_response_larger_than_the_transfer_budget() {
        let result = tokenize_batch_core(TokenizeBatchRequest {
            tokenizer_id: NativeTokenizerId::Cl100kBase,
            artifact_fingerprint: CL100K_FINGERPRINT.to_string(),
            mode: NativeTokenizeMode::Ids,
            texts: vec!["\0".repeat(262_145)],
        });
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("oversized IDs response should be rejected"),
        };

        assert_eq!(
            serde_json::to_value(error).expect("error should serialize"),
            serde_json::json!({
                "code": "response_id_limit_exceeded",
                "message": "Native tokenizer ID response limit exceeded.",
                "limit": 262_144,
                "actual": 262_145,
            })
        );
    }

    #[test]
    fn command_boundary_returns_the_serialized_count_contract() {
        let response = tauri::async_runtime::block_on(tokenize_batch(TokenizeBatchRequest {
            tokenizer_id: NativeTokenizerId::Cl100kBase,
            artifact_fingerprint: CL100K_FINGERPRINT.to_string(),
            mode: NativeTokenizeMode::Count,
            texts: vec!["hello".to_string(), "안녕하세요".to_string()],
        }))
        .expect("command should complete");

        assert_eq!(
            serde_json::to_value(response).expect("response should serialize"),
            serde_json::json!({
                "mode": "count",
                "artifact_fingerprint": CL100K_FINGERPRINT,
                "counts": [1, 5],
            })
        );
    }

    #[test]
    fn empty_batches_and_every_error_position_have_stable_shapes() {
        for mode in [NativeTokenizeMode::Count, NativeTokenizeMode::Ids] {
            let response = tokenize_batch_core(TokenizeBatchRequest {
                tokenizer_id: NativeTokenizerId::Cl100kBase,
                artifact_fingerprint: CL100K_FINGERPRINT.to_string(),
                mode,
                texts: Vec::new(),
            })
            .expect("empty batch should succeed");
            let serialized = serde_json::to_value(response).expect("response should serialize");
            assert_eq!(serialized["mode"], serde_json::to_value(mode).unwrap());
            assert_eq!(
                serialized[if matches!(mode, NativeTokenizeMode::Count) {
                    "counts"
                } else {
                    "ids"
                }],
                serde_json::json!([])
            );
        }

        for error_index in 0..3 {
            let mut texts = vec!["safe".to_string(); 3];
            texts[error_index] = "<|endoftext|>".to_string();
            let error = tokenize_batch_core(TokenizeBatchRequest {
                tokenizer_id: NativeTokenizerId::O200kBase,
                artifact_fingerprint: O200K_FINGERPRINT.to_string(),
                mode: NativeTokenizeMode::Ids,
                texts,
            })
            .expect_err("special token should reject the batch");
            assert_eq!(error.index, Some(error_index));
        }
    }

    #[test]
    #[ignore = "Windows release measurement"]
    fn windows_core_benchmark() {
        fn percentile(samples: &[f64], fraction: f64) -> f64 {
            let mut sorted = samples.to_vec();
            sorted.sort_by(f64::total_cmp);
            sorted[(sorted.len() as f64 * fraction).ceil() as usize - 1]
        }

        fn short_segments(count: usize) -> Vec<String> {
            (0..count)
                .map(|index| {
                    format!("chat segment {index}: user asks a concise tokenizer question.")
                })
                .collect()
        }

        let paragraph =
            "System: answer carefully. User: Explain tokenizer batching with Unicode 안녕하세요 and code `value += 1`. ";
        let mut fixtures = [1, 10, 100, 1_000]
            .into_iter()
            .map(|size| (format!("short-segments-{size}"), short_segments(size)))
            .collect::<Vec<_>>();
        fixtures.push(("prompt-32-kib".to_string(), vec!["P".repeat(32 * 1024)]));
        fixtures.push((
            "realistic-prompt-512-kib".to_string(),
            vec![paragraph
                .repeat((512usize * 1024).div_ceil(paragraph.len()))
                .chars()
                .take(512 * 1024)
                .collect()],
        ));

        let mut tokenizer_results = Vec::new();
        for tokenizer_id in [NativeTokenizerId::Cl100kBase, NativeTokenizerId::O200kBase] {
            let cold_started = Instant::now();
            let cold = tokenize_batch_core(TokenizeBatchRequest {
                tokenizer_id,
                artifact_fingerprint: tokenizer_id.fingerprint().to_string(),
                mode: NativeTokenizeMode::Count,
                texts: vec!["cold singleton initialization".to_string()],
            })
            .unwrap();
            std::hint::black_box(cold);
            let cold_ms = cold_started.elapsed().as_secs_f64() * 1_000.0;

            let mut cases = Vec::new();
            for (fixture, texts) in &fixtures {
                for mode in [NativeTokenizeMode::Count, NativeTokenizeMode::Ids] {
                    let mut samples = Vec::new();
                    for _ in 0..20 {
                        let started = Instant::now();
                        let response = tokenize_batch_core(TokenizeBatchRequest {
                            tokenizer_id,
                            artifact_fingerprint: tokenizer_id.fingerprint().to_string(),
                            mode,
                            texts: texts.clone(),
                        })
                        .unwrap();
                        std::hint::black_box(response);
                        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
                    }
                    cases.push(serde_json::json!({
                        "fixture": fixture,
                        "mode": mode,
                        "samples": samples.len(),
                        "p50Ms": percentile(&samples, 0.5),
                        "p95Ms": percentile(&samples, 0.95),
                    }));
                }
            }
            tokenizer_results.push(serde_json::json!({
                "tokenizerId": tokenizer_id,
                "coldMs": cold_ms,
                "cases": cases,
            }));
        }

        println!(
            "TOKENIZER_CORE_BENCHMARK_JSON={}",
            serde_json::json!({
                "schemaVersion": 1,
                "release": !cfg!(debug_assertions),
                "samplesPerWarmCase": 20,
                "tokenizers": tokenizer_results,
            })
        );
    }
}
