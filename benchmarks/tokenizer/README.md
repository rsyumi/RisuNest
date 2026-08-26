# Tokenizer compatibility evidence

This directory retains the Roadmap 14 K2 literal JavaScript oracle corpus, baseline benchmark, and integrated Windows Tauri benchmark.

## Corpus

`native-tokenizer-corpus.json` contains 82 literal cases for the current cl100k and o200k artifacts:

- 68 exact token-ID cases;
- 14 disallowed-special-token error cases;
- Unicode, UTF-16 boundary, control, source, prompt, and inputs larger than 32 KiB;
- the inspected artifact SHA-256 values and pilot compatibility fingerprints.

The expected IDs are checked in. Tests never derive expected values from a native implementation. `generate-corpus.mjs` records how the fixture was generated from `@dqbd/tiktoken` 1.0.22 and the repository o200k artifact. Its output is an `apply_patch` block so regeneration remains an explicit review step.

Run the provenance and exact-parity checks with:

```powershell
$env:VITE_DISABLE_REALM = 'true'
pnpm test:tokenizer-corpus
```

## Oracle benchmark

Run the retained JavaScript baseline with:

```powershell
$env:VITE_DISABLE_REALM = 'true'
pnpm benchmark:tokenizer:oracle -- --samples 20 --output tokenizer-oracle.json
```

The benchmark first verifies both artifact hashes and every literal corpus result. It then records P50 and P95 for count and ID output over deterministic batches of 1, 10, 100, and 1,000 short segments, one prompt larger than 32 KiB, and six realistic prompt segments.

This output is a JavaScript oracle baseline only. `nativeCandidateMeasured` is always false. Use the integrated Tauri benchmark for native comparisons.

## Windows Tauri benchmark

Run the release benchmark from the repository root with the shared Roadmap 14 Cargo target:

```powershell
$env:VITE_DISABLE_REALM = 'true'
$env:CARGO_TARGET_DIR = 'E:\Programming\Github\RisuNest\.worktrees\_cargo-target-r14'
pnpm benchmark:tokenizer:tauri -- --output "$env:CARGO_TARGET_DIR\k2-tokenizer-windows.json"
```

The runner builds an isolated release Tauri app with `VITE_TOKENIZER_BENCHMARK=true`. That flag installs a narrow WebView-only benchmark seam. The seam proves the JavaScript implementation against the checked-in corpus, executes the same corpus through the real `tokenize_batch` IPC command, and times both implementations inside the same release WebView.

Each implementation receives one untimed warm-up for every fixture and mode. Count results are `number[]` on both paths. ID results are `Uint32Array[]` on both paths, and the native IPC array conversion is included in its measured interval. The Node CDP driver is not timed. WebView garbage collection and heap samples bracket each implementation separately.

The runner owns the Tauri build and app process trees. Normal completion, failure, SIGINT, and SIGTERM stop owned processes and remove the isolated profile unless a normally completed run explicitly uses `--keep-profile`.

The runner does not access live RisuRealm or live account services.

Measure the Rust core without IPC separately:

```powershell
$env:VITE_DISABLE_REALM = 'true'
$env:CARGO_TARGET_DIR = 'E:\Programming\Github\RisuNest\.worktrees\_cargo-target-r14'
pnpm benchmark:tokenizer:core
```
