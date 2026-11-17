import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { runInNewContext } from "node:vm";

const workflow = readFileSync(new URL("../../.github/workflows/release.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const stableWorkflow = readFileSync(new URL("../../.github/workflows/stable-release.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const cacheWorkflow = readFileSync(new URL("../../.github/workflows/release-cache.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const referenceAction = readFileSync(new URL("../../.github/actions/test-references/action.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");

test("all app distribution builds receive the mobile-forwarded key and mandatory native key validation", () => {
  const environment = workflow.slice(workflow.indexOf("\nenv:\n"), workflow.indexOf("\njobs:\n"));
  assert.match(environment, /TAURI_RISUNEST_UPDATE_PUBLIC_KEY: \$\{\{ secrets\.RISUNEST_UPDATE_PUBLIC_KEY \}\}/);
  for (const job of ["app-desktop", "app-android", "app-ios"]) {
    const block = new RegExp(`\\n  ${job}:\\n([\\s\\S]*?)(?=\\n  [\\w-]+:\\n|$)`).exec(workflow)?.[1];
    assert(block, `Missing ${job} job`);
    assert.match(block, /node scripts\/release\/tauri-config\.mjs --release-input release-input\.json --output "\$RUNNER_TEMP\/release-tauri\.json"/);
    assert.match(block, /pnpm exec -- tauri (?:android |ios )?build [^\n]*--config "\$RUNNER_TEMP\/release-tauri\.json"/);
    assert.doesNotMatch(block, /^\s+(?:TAURI_RISUNEST_UPDATE_PUBLIC_KEY|RISUNEST_UPDATE_PUBLIC_KEY):/m);
  }
});

test("README updates follow successful production publication and share its serialization group", () => {
  const block = /\n  update-readme:\n([\s\S]*?)(?=\n  [\w-]+:\n|$)/.exec(workflow)?.[1];
  assert(block);
  assert.match(block, /needs: \[source, release-result\]/);
  const condition = /if: \$\{\{ (.+) \}\}/.exec(block)?.[1];
  assert(condition);
  const permits = (publish, result, cancelled = false) => runInNewContext(condition
    .replace(/cancelled\(\)/g, JSON.stringify(cancelled))
    .replace(/inputs\.publish/g, JSON.stringify(publish))
    .replace(/needs\.release-result\.result/g, JSON.stringify(result)));
  assert.equal(permits(true, "success"), true);
  assert.equal(permits(false, "success"), false);
  assert.equal(permits(true, "success", true), false);
  for (const result of ["failure", "skipped", "cancelled"]) assert.equal(permits(true, result), false);
  assert.match(block, /permissions:\s+contents: write/);
  assert.match(block, /group: risunest-stable-publish\s+queue: max\s+cancel-in-progress: false/);
  assert.match(block, /ref: "?\$\{\{ needs\.source\.outputs\.source_commit \}\}"?/);
  assert.match(block, /persist-credentials: false/);
  assert.match(block, /GITHUB_TOKEN: \$\{\{ github\.token \}\}/);
  assert.match(block, /run: node scripts\/release\/readme\.mjs/);
  assert.doesNotMatch(block, /TAURI_PRIVATE_KEY|TAURI_KEY_PASSWORD|ANDROID_KEYSTORE|continue-on-error/);
});

function assertRequiredVerificationGates(contents, gate = "collect-publish") {
  const preparation = contents.slice(contents.indexOf(`\n  ${gate}:\n`));
  const expression = /if: >-\n([\s\S]*?)\n    needs:/.exec(preparation)?.[1];
  const dependencies = /needs: \[([^\]]+)\]/.exec(preparation)?.[1].split(",").map(value => value.trim());
  assert(expression && dependencies, "Missing preparation gate");
  const shared = ["source", "prepare-draft", "release-tooling-tests", "endpoint-registry-tests", "shared-wasm-tests"];
  const products = {
    app: ["app-web-tests", "app-desktop", "app-android-tests", "app-ios-tests", ...(gate === "collect-publish" ? ["app-android", "app-ios"] : [])],
    sync: ["sync-suite", "sync-gui-tests"],
  };
  const all = [...shared, ...products.app, ...products.sync];
  for (const job of all) assert(dependencies.includes(job), `Missing dependency: ${job}`);
  const permits = (product, results) => runInNewContext(expression
    .replace(/always\(\)/g, "true")
    .replace(/inputs\.publish/g, "true")
    .replace(/inputs\.product/g, JSON.stringify(product))
    .replace(/needs\.prepare-draft\.outputs\.already_published/g, JSON.stringify(gate === "recover-published" ? "true" : "false"))
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

test("failed, cancelled or skipped mandatory checks cannot publish or recover either release", () => {
  for (const gate of ["collect-publish", "recover-published"]) {
    assertRequiredVerificationGates(workflow, gate);
    const start = workflow.indexOf(`\n  ${gate}:\n`);
    const before = workflow.slice(0, start);
    const after = workflow.slice(start);
    assert.throws(() => assertRequiredVerificationGates(before + after.replace("endpoint-registry-tests, ", ""), gate), /Missing dependency/);
    assert.throws(() => assertRequiredVerificationGates(before + after.replace("needs.shared-wasm-tests.result == 'success' &&", ""), gate));
  }
  for (const job of ["endpoint-registry-tests", "shared-wasm-tests"]) {
    const block = new RegExp(`\\n  ${job}:\\n([\\s\\S]*?)(?=\\n  [a-z][\\w-]*:|$)`).exec(workflow)?.[1];
    assert(block);
    assert.match(block, /needs: source/);
    assert.match(block, /ref: "\$\{\{ needs\.source\.outputs\.source_commit \}\}"/);
    assert.doesNotMatch(block, /if: inputs\.product|secrets\./);
  }
});

test("stable tag pushes publish the triggering commit with an explicit tag", () => {
  assert.match(stableWorkflow, /tags:\n\s+- "app-v\*"\n\s+- "sync-v\*"/);
  assert.match(stableWorkflow, /source_ref: \$\{\{ github\.sha \}\}/);
  assert.match(stableWorkflow, /publish: true/);
  assert.match(stableWorkflow, /tag: \$\{\{ github\.ref_name \}\}/);
  assert.match(workflow, /fetch-depth: 0/);
});

test("source validation and downstream builds use the resolved tag and checked commit", () => {
  const source = workflow.slice(workflow.indexOf("\n  source:\n"), workflow.indexOf("\n  release-tooling-tests:\n"));
  const ancestry = source.indexOf('git merge-base --is-ancestor "$(git rev-parse HEAD)" refs/remotes/origin/main');
  assert.ok(ancestry > 0 && ancestry < source.indexOf("      - uses: pnpm/action-setup"),
    "Reject sources outside main before executing checkout-owned tooling");
  const identity = source.slice(source.indexOf("      - id: identity"), source.indexOf("      - name: Validate source release inputs"));
  assert.match(identity, /PRODUCT: \$\{\{ inputs\.product \}\}/);
  assert.match(identity, /TAG: \$\{\{ inputs\.tag \}\}/);
  assert.match(identity, /PUBLISH: \$\{\{ inputs\.publish \}\}/);
  assert.match(source, /tag: \$\{\{ steps\.identity\.outputs\.tag \}\}/);
  assert.match(source, /source_commit: \$\{\{ steps\.identity\.outputs\.source_commit \}\}/);
  const validation = source.slice(source.indexOf("      - name: Validate source release inputs"));
  assert.match(validation, /TAG: \$\{\{ steps\.identity\.outputs\.tag \}\}/);
  assert.match(validation, /SOURCE_COMMIT: \$\{\{ steps\.identity\.outputs\.source_commit \}\}/);
});

test("Windows retained uninstallers use the previous release and an independent signed legacy baseline", () => {
  const fetch = workflow.slice(workflow.indexOf("      - name: Fetch and verify the previous published Windows Sync bundle"), workflow.indexOf("      - name: Apply and roll back the produced macOS Sync app"));
  assert.match(fetch, /verifiedBundle\(previous\.release, previous\.version\)/);
  assert.match(fetch, /verifiedBundle\(await github\.getByTag\("sync-v1\.0\.4"\), "1\.0\.4"\)/);
  assert.match(fetch, /await verifyFileSignature\(bundle, signature, process\.env\.RISUNEST_UPDATE_PUBLIC_KEY\)/);
  assert.match(fetch, /git fetch --no-tags --depth=1 origin "refs\/tags\/\$\(\$tag\):refs\/tags\/\$\(\$tag\)"/);
  assert.match(fetch, /'-LegacyReleasedBundle', \$env:RISUNEST_LEGACY_SYNC_BUNDLE/);
});

test("manual release runs default to a non-publishing main rehearsal", () => {
  const dispatch = workflow.slice(workflow.indexOf("  workflow_dispatch:"), workflow.indexOf("\npermissions:"));
  assert.match(dispatch, /source_ref:\n\s+description:[^\n]+\n\s+type: string\n\s+default: main/);
  assert.match(dispatch, /publish:\n\s+description:[^\n]+\n\s+type: boolean\n\s+default: false/);
  assert.match(dispatch, /tag:\n\s+description:[^\n]+\n\s+type: string\n\s+required: false/);
  assert.match(workflow, /tag: \{ type: string, required: false \}/);
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
  assert.match(tooling, /uses: actions\/setup-java@[a-f0-9]{40} # v5\n\s+if: inputs\.product == 'app'/);
  assert.match(tooling, /name: Clean signing preflight files\n\s+if: always\(\)\n\s+run: >-/);

  const keyFreeSteps = [
    "Compile app desktop binary without signing key",
    "Package app desktop assets",
    "Build and package signed ARM64 APK",
    "Build and package unsigned iPhoneOS ARM64 IPA",
    "Build and package Sync distributions",
  ];
  for (const name of keyFreeSteps) {
    const start = workflow.indexOf(`      - name: ${name}\n`);
    assert.notEqual(start, -1, `Missing key-free step: ${name}`);
    const end = workflow.indexOf("\n      - name:", start + 1);
    const step = workflow.slice(start, end === -1 ? workflow.length : end);
    assert.doesNotMatch(step, /TAURI_SIGNING_PRIVATE_KEY/);
  }
  for (const name of [
    "Bundle app desktop assets",
    "Sign final app desktop assets",
    "Sign final ARM64 APK",
    "Sign final iPhoneOS ARM64 IPA",
    "Sign final Sync distributions",
  ]) {
    const start = workflow.indexOf(`      - name: ${name}\n`);
    assert.notEqual(start, -1, `Missing signing step: ${name}`);
    const end = workflow.indexOf("\n      - name:", start + 1);
    assert.match(workflow.slice(start, end === -1 ? workflow.length : end), /TAURI_SIGNING_PRIVATE_KEY/);
  }
});

test("the reusable release forwards only its eight declared secrets", () => {
  const names = [
    "TAURI_PRIVATE_KEY",
    "TAURI_KEY_PASSWORD",
    "RISUNEST_UPDATE_PUBLIC_KEY",
    "ANDROID_KEYSTORE_BASE64",
    "ANDROID_KEYSTORE_PASSWORD",
    "ANDROID_KEY_ALIAS",
    "ANDROID_KEY_PASSWORD",
    "RISUNEST_DEFAULT_REGISTRY_URL",
  ];
  assert.doesNotMatch(stableWorkflow, /secrets: inherit/);
  const callContract = workflow.slice(workflow.indexOf("  workflow_call:\n"), workflow.indexOf("  workflow_dispatch:\n"));
  const forwarding = stableWorkflow.slice(stableWorkflow.indexOf("    secrets:\n"));
  for (const name of names) {
    assert.match(callContract, new RegExp(`\\n      ${name}:`));
    assert.match(forwarding, new RegExp(`${name}: \\$\\{\\{ secrets\\.${name} \\}\\}`));
  }
  assert.equal((forwarding.match(/^      [A-Z][A-Z0-9_]+:/gm) ?? []).length, names.length);
});

test("remote actions inside the release signing boundary use verified commit SHAs", () => {
  const verified = new Map([
    ["actions/checkout#v6", "d23441a48e516b6c34aea4fa41551a30e30af803"],
    ["actions/checkout#v4", "11d5960a326750d5838078e36cf38b85af677262"],
    ["actions/setup-node#v6", "249970729cb0ef3589644e2896645e5dc5ba9c38"],
    ["actions/setup-java#v5", "b6effb05e454b25005698d916606bdc6ffcbf961"],
    ["actions/setup-python#v6", "ece7cb06caefa5fff74198d8649806c4678c61a1"],
    ["actions/cache/save#v5", "caa296126883cff596d87d8935842f9db880ef25"],
    ["actions/cache/restore#v5", "caa296126883cff596d87d8935842f9db880ef25"],
    ["actions/download-artifact#v4", "d3f86a106a0bac45b974a628896c90dbdf5c8093"],
    ["actions/upload-artifact#v4", "ea165f8d65b6e75b540449e92b4886f43607fa02"],
    ["pnpm/action-setup#v5", "fc06bc1257f339d1d5d8b3a19a8cae5388b55320"],
    ["dtolnay/rust-toolchain#1.97.1", "4716b85f2fac3e324e64fa2810f6b5c3905760a5"],
    ["android-actions/setup-android#v3", "9fc6c4e9069bf8d3d10b2204b1fb8f6ef7065407"],
  ]);
  for (const contents of [workflow, referenceAction, cacheWorkflow]) {
    const remoteUses = [...contents.matchAll(/^\s*-?\s*uses:\s+([^\s#]+)(?:\s+#\s*(\S+))?/gm)]
      .filter(match => !match[1].startsWith("./"));
    assert(remoteUses.length > 0);
    for (const match of remoteUses) {
      const [action, reference] = match[1].split("@");
      assert.match(reference ?? "", /^[a-f0-9]{40}$/, `${action} is not pinned`);
      assert(match[2], `${action} is missing its readable version comment`);
      assert.equal(reference, verified.get(`${action}#${match[2]}`), `${action} is not a verified pin`);
    }
  }
});

test("Android prepares Gradle before builds and bounds SDK and download retries", () => {
  const android = readFileSync(new URL("../../.github/workflows/android-check.yml", import.meta.url), "utf8").replace(/\r\n/g, "\n");
  const jobs = [android, ...["app-android-tests", "app-android"].map(name => {
    const start = workflow.indexOf(`\n  ${name}:\n`);
    return workflow.slice(start, workflow.indexOf("\n  app-ios", start));
  })];
  for (const job of jobs) {
    const prepare = job.indexOf("bash scripts/retry-command.sh bash src-tauri/gen/android/gradlew --version");
    const build = job.search(/pnpm (?:exec -- tauri android build|android:build:arm64)/);
    assert.ok(prepare >= 0 && build > prepare, "Gradle download must precede the real build");
    assert.match(job, /bash scripts\/retry-command.sh sdkmanager /);
  }
  for (const step of [...workflow.split(/^      - /m), ...android.split(/^      - /m)]) {
    if (!step.includes("bash scripts/retry-command.sh")) continue;
    assert.match(step, /timeout-minutes: \d+/);
    assert.doesNotMatch(step, /retry-command\.sh (?:pnpm|cargo)/);
    if (step.includes("gh release download")) assert.match(step, /--clobber/);
  }
});

test("iOS portable source tests run before the simulator release gate", () => {
  const start = workflow.indexOf("\n  app-ios-tests:\n");
  const job = workflow.slice(start, workflow.indexOf("\n  sync-gui-tests:\n", start));
  const verify = contents => {
    const install = contents.indexOf("pnpm install --frozen-lockfile");
    const swift = contents.indexOf("pnpm ios:test:portable-source");
    const simulator = contents.indexOf("bash scripts/ios-native-tests.sh");
    assert.ok(install >= 0 && swift > install && simulator > swift);
  };
  verify(job);
  assert.throws(() => verify(job.replace("pnpm ios:test:portable-source", "")));
});

test("APT packages install through the bounded mirror fallback", () => {
  for (const name of ["release", "native-check", "linux-package-check", "pr-check"]) {
    const contents = readFileSync(new URL(`../../.github/workflows/${name}.yml`, import.meta.url), "utf8");
    assert.doesNotMatch(contents, /apt-get/, name);
    const steps = contents.split(/^      - /m).filter(step => step.includes("run: bash scripts/install-apt-packages.sh"));
    assert.ok(steps.length >= 1, name);
    for (const step of steps) assert.match(step, /timeout-minutes: 20\r?\n/, name);
  }
});
