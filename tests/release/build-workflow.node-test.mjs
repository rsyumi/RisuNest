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

test("Windows installer failure checks run after the job has cached NSIS", () => {
  const packageStep = workflow.indexOf("node server/manager/install/package.mjs");
  const installerTest = workflow.indexOf("server/manager/install/windows.test.ps1");
  assert(packageStep >= 0 && installerTest > packageStep);
});

test("both native installer rollback harnesses are required release gates", () => {
  assert.match(workflow, /if: runner\.os == 'Linux'\n\s+run: bash server\/manager\/install\/install\.test\.sh/);
  assert.match(workflow, /if: matrix\.os == 'windows'\n\s+shell: pwsh\n\s+run: pwsh -NoProfile -File server\/manager\/install\/windows\.test\.ps1/);
});
