import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflow = readFileSync(new URL("../../.github/workflows/release.yml", import.meta.url), "utf8");
const cacheWorkflow = readFileSync(new URL("../../.github/workflows/release-cache.yml", import.meta.url), "utf8");

test("release source input uses one run timestamp instead of the commit timestamp", () => {
  assert.match(workflow, /published_at=\$\(date -u/);
  assert.doesNotMatch(workflow, /git show[^\n]*--format=%cI/);
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
  const sourceAndTests = workflow.slice(workflow.indexOf("\n  source:\n"), workflow.indexOf("\n  prepare-draft:\n"));
  assert.doesNotMatch(sourceAndTests, /TAURI_PRIVATE_KEY|ANDROID_KEYSTORE_BASE64/);
  assert.equal((workflow.match(/^\s+ANDROID_KEYSTORE_BASE64:/gm) ?? []).length, 1);
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
  assert.match(syncSuite, /if: matrix\.os == 'linux'[\s\S]*loginctl enable-linger[\s\S]*asset\.download\.arch === process\.argv\[1\]/);
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

test("Windows release gates run every scheduled update lifecycle test exactly", () => {
  for (const name of [
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
