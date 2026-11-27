# Generation memory measurements

This harness runs real coordinator, canonical capture, conversation session, streaming display scheduler, history-window selection, listener projection, and IndexedDB store modules with synthetic fixtures. The store uses fake-indexeddb, so elapsed times describe Node execution rather than native storage or device performance.

The fixture uses the installed `@dqbd/tiktoken` `cl100k_base` encoding. Reports contain exact per-message token sums, message count, characters, and UTF-8 bytes. The response has 20,000 tokens. Streaming retention uses an equivalent-length synthetic 51,210-character response, rather than the unavailable text of the earlier investigation fixture.

Run all measurement harnesses with `pnpm test:harness`. To run this matrix alone:

```powershell
$env:GENERATION_MEMORY_LABEL = 'baseline'
$env:GENERATION_MEMORY_VOLUME = '0'
pnpm exec vitest run --project harness benchmarks/generation-memory/generationMemory.test.ts --maxWorkers=1
$env:GENERATION_MEMORY_VOLUME = '1'
pnpm exec vitest run --project harness benchmarks/generation-memory/generationMemory.test.ts --maxWorkers=1
```

`GENERATION_MEMORY_SMOKE=1` selects small histories. `GENERATION_MEMORY_GROUP` selects `all`, `buffered`, `delivery`, or `streaming`. `GENERATION_MEMORY_OUTPUT` overrides the default `.tmp/generation-memory` output directory. Keep timing runs separate from other builds and tests. Volume runs execute the same functions with observation enabled and are unsuitable for timing comparisons.

Each output records hashes of the measured product sources. Buffered cases cover 100k, 400k, and 1.6M history tokens, complete and windowed persistence, first flush after promotion, subsequent flush, eight pending changes, zero/one full-object listener, and an independent 1 MiB conversation metadata variant. A separate case delivers the same 20k-token final response in 200 increments at 1.6M history tokens. Streaming cases cover 200/400/800 updates at 20 ms simulated intervals in off/normal modes, with no periodic requests, 2-second requests, and a commit blocked until completion.

The response checkpoint requests in this harness are an explicit experiment. They do not claim to exercise response-layer eligibility or lifecycle scheduling. Correctness tests for those production boundaries are separate. The coordinator debounce clock does not advance, representing continuous activity without a 500 ms quiet gap.

Logical volumes count string characters passed through real `structuredClone` attempts and `JSON.stringify` calls. A failed structured clone followed by the product fallback counts once. These values are not heap sizes. Projection and acknowledgement are attributed separately from the full flush. Windowed projection includes its real bounded store page reads. JSON command byte counts describe the actual `WorkingSetCommit` passed to the store; native wire bytes are unknown. The dedicated config below counts calls to message, pending-prefix, and final conversation comparison helpers, with the original function bodies intact. The default config does not instrument comparisons and marks that fact explicitly. Renderer peak memory and native/device measurements are unknown and emitted as null where applicable.

The fixture applies a response through a complete `ActiveConversationSession`. For the windowed route it also stages each changed range in the production overlay before notifying the coordinator. The timing probes cover planning and acknowledging that overlay. History-window selection is measured independently. The no-op parser case measures complete-session traversal and isolation. The separate generation-send harness runs the send pipeline, including prompt construction and parser admission.

Retention inspection reads the coordinator's pending events, session residency's dirty records, and visible dirty messages in the windowed overlay without cloning the bodies. It reports logical volume per owner before each requested checkpoint, after acknowledgement (or while blocked), and at completion. It excludes frozen copies held only by an in-flight commit, UI, provider, and runtime allocations. Final stored text is read back through the real store and checked, and all scenarios use local synthetic data with no provider requests.

To count comparison helpers, run the dedicated harness config with volume observation enabled:

```powershell
$env:GENERATION_MEMORY_VOLUME = '1'
pnpm exec vitest run --config benchmarks/generation-memory/vitest.config.ts
```

The dedicated config can replay a preserved local source snapshot using `GENERATION_MEMORY_BASELINE_SOURCE`. The snapshot folder must mirror product-relative paths for every source listed in the report. The report hashes those loaded files. This is a harness build transform, and it never edits production source files. Leave the variable unset to measure the current working tree. Timing evidence uses the default config without this transform.
