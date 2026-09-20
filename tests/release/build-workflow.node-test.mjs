import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { runInNewContext } from "node:vm";

const workflow = readFileSync(new URL("../../.github/workflows/release.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const stableWorkflow = readFileSync(new URL("../../.github/workflows/stable-release.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const cacheWorkflow = readFileSync(new URL("../../.github/workflows/release-cache.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const checkWorkflow = readFileSync(new URL("../../.github/workflows/release-check.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");

test("both products and pull requests run shared native signature and catalog tests", () => {
  const commonTests = workflow.slice(workflow.indexOf("\n  release-tooling-tests:\n"), workflow.indexOf("\n  app-web-tests:\n"));
  assert.doesNotMatch(commonTests, /\n    if: inputs\.product/);
  for (const contents of [commonTests, checkWorkflow]) {
    assert.match(contents, /uses: dtolnay\/rust-toolchain@1\.97\.1/);
  }
  assert.match(commonTests, /pnpm test:protocol/);
  assert.match(commonTests, /pnpm test:node/);
  assert.match(commonTests, /pnpm test --project harness/);
  assert.doesNotMatch(commonTests, /pnpm test:release/);
  assert.match(checkWorkflow, /cargo test --manifest-path crates\/release-update\/Cargo\.toml --release --locked/);
  assert.match(workflow, /needs: \[source, release-tooling-tests,/);
});

function assertRequiredVerificationGates(contents) {
  const preparation = contents.slice(contents.indexOf("\n  prepare-draft:\n"));
  const expression = /if: >-\n([\s\S]*?)\n    needs:/.exec(preparation)?.[1];
  const dependencies = /needs: \[([^\]]+)\]/.exec(preparation)?.[1].split(",").map(value => value.trim());
  assert(expression && dependencies, "Missing preparation gate");
  const shared = ["source", "release-tooling-tests", "endpoint-registry-tests", "shared-wasm-tests"];
  const products = {
    app: ["app-web-tests", "app-native-tests", "app-android-tests", "app-ios-tests"],
    sync: ["sync-tests", "sync-gui-tests"],
  };
  const all = [...shared, ...products.app, ...products.sync];
  for (const job of all) assert(dependencies.includes(job), `Missing dependency: ${job}`);
  const permits = (product, results) => runInNewContext(expression
    .replace(/always\(\)/g, "true")
    .replace(/inputs\.product/g, JSON.stringify(product))
    .replace(/needs\.([\w-]+)\.result/g, (_, job) => JSON.stringify(results[job])),
  Object.create(null), { timeout: 100, contextCodeGeneration: { strings: false, wasm: false } });
  for (const product of Object.keys(products)) {
    const baseline = Object.fromEntries(all.map(job => [job, "skipped"]));
    for (const job of [...shared, ...products[product]]) baseline[job] = "success";
    assert.equal(permits(product, baseline), true, `${product} successful verification`);
    for (const job of [...shared, ...products[product]]) {
      for (const result of ["failure", "cancelled", "skipped"]) {
        assert.equal(permits(product, { ...baseline, [job]: result }), false, `${product}: ${job} ${result}`);
      }
    }
  }
}

test("failed or cancelled mandatory checks cannot prepare either release", () => {
  assertRequiredVerificationGates(workflow);
  for (const job of ["endpoint-registry-tests", "shared-wasm-tests"]) {
    assert.throws(() => assertRequiredVerificationGates(workflow.replace(`${job}, `, "")), /Missing dependency/);
    assert.throws(() => assertRequiredVerificationGates(workflow.replace(new RegExp(`needs\\.${job}\\.result == 'success' &&\\s*`), "")));
    const block = new RegExp(`\\n  ${job}:\\n([\\s\\S]*?)(?=\\n  [a-z][\\w-]*:|$)`).exec(workflow)?.[1];
    assert(block);
    assert.match(block, /needs: source/);
    assert.match(block, /ref: "\$\{\{ needs\.source\.outputs\.source_commit \}\}"/);
    assert.doesNotMatch(block, /if: inputs\.product|secrets\./);
  }
});

test("WASM jobs verify native artifacts afterward and Android includes barcode tests", () => {
  assert.match(workflow, /pnpm test:wasm[\s\S]*dbus-run-session -- bash scripts\/linux-native-tests\.sh --lib external_storage/);
  assert.match(workflow, /:app:testLowMemorySafCopy :tauri-plugin-barcode-scanner:testDebugUnitTest/);
  assert.match(workflow, /pnpm test --project app --project app-extended/);
});

test("release source input uses one run timestamp instead of the commit timestamp", () => {
  assert.match(workflow, /published_at=\$\(date -u/);
  assert.doesNotMatch(workflow, /git show[^\n]*--format=%cI/);
});

test("stable tags publish automatically only from commits contained in main", () => {
  assert.match(stableWorkflow, /tags:\n\s+- "app-v\*"\n\s+- "sync-v\*"/);
  assert.match(stableWorkflow, /source_ref: \$\{\{ github\.sha \}\}/);
  assert.match(stableWorkflow, /publish: true/);
  assert.match(workflow, /fetch-depth: 0/);
  assert.match(workflow, /if \[\[ "\$GITHUB_EVENT_NAME" == "push" \]\]/);
  assert.match(workflow, /test "\$GITHUB_REF_TYPE" = "tag"/);
  assert.match(workflow, /git merge-base --is-ancestor "\$source_commit" refs\/remotes\/origin\/main/);
  assert.match(workflow, /git rev-parse "\$TAG\^\{commit\}"/);
});

test("manual release runs default to a non-publishing main rehearsal", () => {
  const dispatch = workflow.slice(workflow.indexOf("  workflow_dispatch:"), workflow.indexOf("\npermissions:"));
  assert.match(dispatch, /source_ref:\n\s+description:[^\n]+\n\s+type: string\n\s+default: main/);
  assert.match(dispatch, /publish:\n\s+description:[^\n]+\n\s+type: boolean\n\s+default: false/);
});

test("stable release runs serialize per tag while different tags can build together", () => {
  assert.match(workflow, /group: risunest-release-\$\{\{ inputs\.tag \}\}/);
  assert.match(workflow, /group: risunest-stable-publish/);
  assert.match(workflow, /queue: max/);
  assert.match(workflow, /cancel-in-progress: false/);
});

test("private signing material is scoped to signing steps", () => {
  const globalEnvironment = workflow.slice(workflow.indexOf("\nenv:\n"), workflow.indexOf("\njobs:\n"));
  assert.doesNotMatch(globalEnvironment, /TAURI_SIGNING_PRIVATE_KEY|ANDROID_KEYSTORE/);
  const source = workflow.slice(workflow.indexOf("\n  source:\n"), workflow.indexOf("\n  release-tooling-tests:\n"));
  assert.doesNotMatch(source, /TAURI_PRIVATE_KEY|ANDROID_KEYSTORE_BASE64/);
  const tooling = workflow.slice(workflow.indexOf("\n  release-tooling-tests:\n"), workflow.indexOf("\n  app-web-tests:\n"));
  const appPreflight = tooling.slice(tooling.indexOf("      - name: Preflight app signing inputs\n"),
    tooling.indexOf("      - name: Preflight Sync signing inputs\n"));
  const syncPreflight = tooling.slice(tooling.indexOf("      - name: Preflight Sync signing inputs\n"),
    tooling.indexOf("      - name: Clean signing preflight files\n"));
  for (const secret of [
    "TAURI_PRIVATE_KEY",
    "TAURI_KEY_PASSWORD",
    "RISUNEST_UPDATE_PUBLIC_KEY",
  ]) {
    assert.match(appPreflight, new RegExp(`secrets\\.${secret}`));
    assert.match(syncPreflight, new RegExp(`secrets\\.${secret}`));
  }
  for (const secret of [
    "ANDROID_KEYSTORE_BASE64",
    "ANDROID_KEYSTORE_PASSWORD",
    "ANDROID_KEY_ALIAS",
    "ANDROID_KEY_PASSWORD",
  ]) {
    assert.match(appPreflight, new RegExp(`secrets\\.${secret}`));
    assert.doesNotMatch(syncPreflight, new RegExp(`secrets\\.${secret}`));
  }
  assert.doesNotMatch(appPreflight, /secrets\.RISUNEST_DEFAULT_REGISTRY_URL/);
  assert.match(syncPreflight, /secrets\.RISUNEST_DEFAULT_REGISTRY_URL/);
  assert.match(tooling, /uses: actions\/setup-java@v5\n\s+if: inputs\.product == 'app'/);
  assert.match(tooling, /name: Clean signing preflight files\n\s+if: always\(\)/);
});

test("release caches retain downloads without unpacked dependency trees", () => {
  for (const contents of [workflow, cacheWorkflow]) {
    assert.doesNotMatch(contents, /^\s+~\/.cargo\/registry\s*$/m);
    assert.doesNotMatch(contents, /^\s+~\/.cargo\/git\s*$/m);
    assert.match(contents, /~\/.cargo\/registry\/cache/);
    assert.match(contents, /~\/.cargo\/registry\/index/);
    assert.match(contents, /~\/.cargo\/git\/db/);
  }
  assert.match(cacheWorkflow, /release-cargo-1\.97\.1-/);
  const savedPaths = [...cacheWorkflow.matchAll(/uses: actions\/cache\/save@v5\s+with:\s+path: ([\s\S]*?)\s+key:/g)]
    .map((match) => match[1])
    .join("\n");
  assert.equal((cacheWorkflow.match(/uses: actions\/cache\/save@v5/g) ?? []).length, 2);
  assert.doesNotMatch(savedPaths, /src-tauri\/target|node_modules/);
});

test("the independent Sync GUI lockfile bypasses outer workspace discovery", () => {
  assert.match(workflow, /pnpm --dir server\/manager\/gui --ignore-workspace install --frozen-lockfile/);
  assert.match(workflow, /pnpm --dir server\/manager\/gui --ignore-workspace check/);
  assert.match(workflow, /pnpm --dir server\/manager\/gui --ignore-workspace test/);
  assert.match(cacheWorkflow, /pnpm --dir server\/manager\/gui --ignore-workspace fetch --frozen-lockfile/);
  assert.doesNotMatch(workflow, /pnpm install --dir server\/manager\/gui/);
});

test("iPhoneOS production uses the lab-verified Xcode and Tauri IPA path", () => {
  assert.equal((workflow.match(/\/Applications\/Xcode_26\.3\.app\/Contents\/Developer/g) ?? []).length, 2);
  assert.doesNotMatch(workflow, /Xcode_16\.4|ios build[^\n]*--archive-only/);
  assert.match(workflow, /tauri ios build[^\n]*--no-sign/);
});

test("pnpm preserves the Cargo argument separator for every Tauri build", () => {
  assert.doesNotMatch(workflow, /pnpm exec tauri/);
  assert.equal((workflow.match(/pnpm exec -- tauri/g) ?? []).length, 5);
  assert.equal((workflow.match(/pnpm exec -- tauri[^\n]* -- --locked/g) ?? []).length, 5);
});

test("Android jobs preserve the committed shell and let Tauri generate ignored build files", () => {
  assert.doesNotMatch(workflow, /pnpm android:init|tauri android init/);
  assert.equal((workflow.match(/name: Verify the committed Android shell/g) ?? []).length, 2);
  assert.match(workflow, /Generate Android build files through a real agent build[\s\S]*tauri android build --debug --apk --target aarch64/);
  assert.equal((workflow.match(/git diff --exit-code -- src-tauri\/gen\/android/g) ?? []).length, 4);
  const releaseJob = workflow.slice(workflow.indexOf("\n  app-android:\n"), workflow.indexOf("\n  app-ios:\n"));
  assert.match(releaseJob, /export PATH="\$ANDROID_HOME\/build-tools\/36\.0\.0:\$PATH"/);
});

test("Windows installer failure checks run after the job has cached NSIS", () => {
  const packageStep = workflow.indexOf("node server/manager/install/package.mjs");
  const installerTest = workflow.indexOf("server/manager/install/windows.test.ps1");
  assert(packageStep >= 0 && installerTest > packageStep);
});

test("both macOS app legs run the produced archive updater before upload", () => {
  const start = workflow.indexOf("\n  app-desktop:\n");
  const end = workflow.indexOf("\n  app-android:\n");
  assert(start >= 0 && end > start);
  const desktop = workflow.slice(start, end);
  const macRows = desktop.split("\n").filter((line) => /runner: macos-/.test(line));
  assert.equal(macRows.length, 2);
  const gateStart = desktop.indexOf("name: Verify the produced macOS archive with the production updater");
  const uploadStart = desktop.indexOf("name: Upload final assets to draft");
  assert(gateStart > desktop.indexOf("node scripts/release/package-app.mjs"));
  assert(uploadStart > gateStart);
  const gate = desktop.slice(gateStart, uploadStart);
  const gatedOs = /if: matrix\.os == '([^']+)'/.exec(gate)?.[1];
  for (const row of macRows) assert.equal(/\bos: ([a-z]+)/.exec(row)?.[1], gatedOs);
  assert.match(gate, /--package tauri-plugin-updater/);
  assert.match(gate, /produced_macos_archive_replaces_disposable_app_and_preserves_sibling_data -- --ignored --exact --nocapture/);
  assert.match(gate, /test result: ok\. 1 passed; 0 failed; 0 ignored;/);
});

test("both native installer rollback harnesses are required release gates", () => {
  assert.match(workflow, /if: runner\.os == 'Linux'\n\s+run: bash server\/manager\/install\/install\.test\.sh/);
  assert.match(workflow, /if: matrix\.os == 'windows'\n\s+shell: pwsh\n\s+run: pwsh -NoProfile -File server\/manager\/install\/windows\.test\.ps1/);
});

test("both Linux release packages pass the real managed installer transaction", () => {
  const start = workflow.indexOf("\n  sync-suite:\n");
  const end = workflow.indexOf("\n  collect-publish:\n");
  assert(start >= 0 && end > start);
  const syncSuite = workflow.slice(start, end);
  const packageStep = syncSuite.indexOf("node server/manager/install/package.mjs");
  const integrationStep = syncSuite.indexOf("server/manager/install/managed-install.integration.test.sh");
  const uploadStep = syncSuite.indexOf("node scripts/release/upload-build.mjs");
  assert(packageStep >= 0 && integrationStep > packageStep && uploadStep > integrationStep);
  assert.match(syncSuite, /env -u XDG_CONFIG_HOME -u XDG_DATA_HOME bash server\/manager\/install\/managed-install\.integration\.test\.sh/);
  assert.match(syncSuite, /if: matrix\.os == 'linux'[\s\S]*loginctl enable-linger[\s\S]*asset\.download\.arch === process\.argv\[1\]/);
});

test("both macOS Sync legs verify whole-app apply and rollback before upload", () => {
  const start = workflow.indexOf("\n  sync-suite:\n");
  const end = workflow.indexOf("\n  collect-publish:\n");
  assert(start >= 0 && end > start);
  const suite = workflow.slice(start, end);
  const macRows = suite.split("\n").filter((line) => /runner: macos-/.test(line));
  assert.equal(macRows.length, 2);
  const gateStart = suite.indexOf("name: Apply and roll back the produced macOS Sync app");
  const uploadStart = suite.indexOf("name: Upload final assets to draft");
  assert(gateStart > suite.indexOf("node scripts/release/build.mjs"));
  assert(uploadStart > gateStart);
  const gate = suite.slice(gateStart, uploadStart);
  const gatedOs = /if: matrix\.os == '([^']+)'/.exec(gate)?.[1];
  for (const row of macRows) assert.equal(/\bos: ([a-z]+)/.exec(row)?.[1], gatedOs);
  assert.match(gate, /RISUNEST_TEST_RUST_TARGET: \$\{\{ matrix\.target \}\}/);
  assert.match(gate, /macos-update\.sh "\$managed" "\$managed\.sig" "\$inventory"/);
  assert.match(gate, /test result: ok\. 1 passed; 0 failed; 0 ignored;/);
});

test("raw Sync release binaries require the embedded update public key", () => {
  const syncBuild = workflow.slice(
    workflow.indexOf("Build console, background and GUI executables once"),
    workflow.indexOf("Run synthetic Windows NSIS install failure and rollback checks"),
  );
  assert.match(syncBuild, /RISUNEST_UPDATE_PUBLIC_KEY: \$\{\{ secrets\.RISUNEST_UPDATE_PUBLIC_KEY \}\}/);
  assert.match(syncBuild, /test -n "\$RISUNEST_UPDATE_PUBLIC_KEY"/);
  assert.match(syncBuild, /node server\/manager\/install\/build\.mjs/);
});

test("Windows release gates run every detached update helper lifecycle test exactly", () => {
  for (const name of [
    "no_claim_helper_task_is_removed_without_consuming_recovery_state",
    "helper_replaces_and_restarts_an_isolated_live_server",
    "helper_verifies_a_stopped_replacement_then_restores_stopped_intent",
    "interrupted_files_recovery_outlives_the_active_installed_manager",
    "helper_recovers_the_live_server_when_its_parent_does_not_exit",
    "helper_rolls_back_a_corrupt_live_replacement_and_restarts_the_old_server",
    "installer_guard_records_restart_failure_after_accepted_corrupt_server_start",
  ]) assert.match(workflow, new RegExp(`'${name}'`));
  assert.match(workflow, /--test update_helper \$test -- --ignored --exact --nocapture/);
  assert.match(workflow, /test result: ok\\\. 1 passed; 0 failed; 0 ignored;/);
});
