# Tokenizer compatibility evidence

This directory retains the safe output of the Roadmap 14 K2 pilot: a literal JavaScript oracle corpus and a repeatable oracle benchmark baseline.

It does not add or register a Tauri tokenizer command. It does not link a native tokenizer implementation into the application, change TypeScript routing, or change a production default. The previous native pilot remains rejected for production because physical Android latency, memory, CPU, and jank evidence is unavailable.

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

This output is a JavaScript oracle baseline only. `nativeCandidateMeasured` is always false. It cannot authorize native production routing. A future native candidate must compare the same literal corpus and batch shapes through real Tauri IPC on Windows and a physical Android device, then satisfy the Roadmap 14 adoption gates in a separate change.
