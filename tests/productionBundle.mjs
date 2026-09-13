import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";

const forbiddenMarkers = [
  "__RISUNEST_LINUX_BENCHMARK__",
  "__RISUNEST_TOKENIZER_BENCHMARK__",
  "__streamingSmoke",
  "__testResponsesAPI",
  "resetOpenedFileListenersForTest",
  "VITE_TOKENIZER_BENCHMARK",
  "VITE_STREAMING_SMOKE",
  "RisuPeerCloneBridge",
  "peer_clone_prepare",
  "peer_delta_prepare",
  "peer_bidirectional_sync",
  "device_sync_start",
  "RISUNESTLOSSLESS",
  ".risulossless",
  "native_lossless_handoff_cleanup",
  "restore-lossless-backup",
  "export-lossless-backup",
  "install_python",
  "install_pip",
  "post_py_install",
  "install_py_dependencies",
  "run_py_server",
  "check_requirements_local",
  "localhost:10026",
  "tokenizeGGUFModel",
];

export function isVerificationModule(id) {
  const normalized = id.replaceAll("\\", "/").split("?")[0];
  if (normalized.includes("/node_modules/")) {
    return /\/(?:@vitest|vitest|happy-dom|fake-indexeddb|fast-check)\//.test(
      normalized,
    );
  }
  return /(?:^|\/)(?:benchmarks|tests|__tests__|__fixtures__)(?:\/|$)|\.(?:test|spec|bench|testSupport)\.[cm]?[jt]sx?$/.test(
    normalized,
  );
}

function isRemovedModule(id) {
  const normalized = id.replaceAll("\\", "/").split("?")[0];
  return (
    /\/src\/ts\/storage\/sync\/(?:peer[A-Z]|deviceSync|bidirectionalSyncPlan|conflictBackupGate)/.test(
      normalized,
    ) ||
    /\/DeviceSyncSettings\.svelte$/.test(normalized) ||
    /\/losslessBackupFileRoute[^/]*$/.test(normalized) ||
    /\/src\/ts\/process\/models\/local\.ts$/.test(normalized)
  );
}

export function assertProductionBundle(output) {
  let javascriptFiles = 0;
  let sourceMaps = 0;
  for (const item of output) {
    if (item.type === "chunk") {
      for (const id of Object.keys(item.modules)) {
        assert.ok(
          !isVerificationModule(id) && !isRemovedModule(id),
          `Verification module in ${item.fileName}: ${id}`,
        );
      }
    }
    if (/\.[cm]?js$/.test(item.fileName)) {
      javascriptFiles++;
      const code = item.type === "chunk" ? item.code : String(item.source);
      for (const marker of forbiddenMarkers)
        assert.ok(
          !code.includes(marker),
          `Verification marker in ${item.fileName}: ${marker}`,
        );
    }
    if (item.type === "asset" && item.fileName.endsWith(".map")) {
      sourceMaps++;
      const map = JSON.parse(String(item.source));
      for (const source of map.sources ?? [])
        assert.ok(
          !isVerificationModule(source) && !isRemovedModule(source),
          `Verification source in ${item.fileName}: ${source}`,
        );
      for (const content of map.sourcesContent ?? []) {
        for (const marker of forbiddenMarkers)
          assert.ok(
            !content?.includes(marker),
            `Verification source text in ${item.fileName}: ${marker}`,
          );
      }
    }
  }
  assert.ok(javascriptFiles > 0, "No JavaScript output was checked");
  return { javascriptFiles, sourceMaps };
}

async function main() {
  const mode = process.argv[2] ?? "desktop";
  assert.ok(
    ["desktop", "android", "production"].includes(mode),
    "Expected desktop, android or production",
  );
  const { build } = await import("vite");
  // Removed legacy switches must not be able to turn a product build into a harness.
  process.env.VITE_TOKENIZER_BENCHMARK = "true";
  process.env.VITE_STREAMING_SMOKE = "true";
  delete process.env.TAURI_ENV_PLATFORM;
  delete process.env.TAURI_ENV_DEBUG;
  const result = await build({
    mode,
    logLevel: "error",
    build: {
      write: false,
      copyPublicDir: false,
      reportCompressedSize: false,
      ...(mode === "production" ? { sourcemap: true } : {}),
    },
  });
  const output = (Array.isArray(result) ? result : [result]).flatMap(
    (result) => result.output,
  );
  console.log(JSON.stringify({ mode, ...assertProductionBundle(output) }));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  await main();
