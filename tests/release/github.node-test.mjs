import assert from "node:assert/strict";
import test from "node:test";
import { GitHubReleases } from "../../scripts/release/github.mjs";

function client(responses) {
  const calls = [];
  const github = new GitHubReleases("synthetic-token", async (url, options) => {
    calls.push({ url, ...options });
    assert.ok(responses.length, "unexpected GitHub request");
    return responses.shift();
  });
  return { github, calls };
}
const json = (value, status = 200) => new Response(JSON.stringify(value), { status });

test("draft creation resolves annotated tags and records the tested commit", async () => {
  const commit = "a".repeat(40);
  const { github, calls } = client([json({}, 404), json({ object: { type: "tag", sha: "b".repeat(40) } }),
    json({ object: { type: "commit", sha: commit } }), json({ id: 7, draft: true })]);
  assert.equal((await github.prepareDraft("app-v1.0.0", commit, "Release notes")).id, 7);
  assert.deepEqual(JSON.parse(calls.at(-1).body), { tag_name: "app-v1.0.0", target_commitish: commit,
    name: "app-v1.0.0", body: "Release notes", draft: true, prerelease: false, make_latest: "false" });
});

test("mismatched tag commits and API errors cannot create a release", async () => {
  const { github, calls } = client([json({}, 404), json({ object: { type: "commit", sha: "b".repeat(40) } })]);
  await assert.rejects(github.prepareDraft("app-v1.0.0", "a".repeat(40)), /tag-commit-mismatch/);
  assert.ok(calls.every(call => call.method === "GET"));
  const unavailable = client([json({ message: "synthetic secret" }, 403)]);
  await assert.rejects(unavailable.github.getLatest(), error => error.message === "github-get-403");
});

test("an existing draft still requires the tag to resolve to the tested commit", async () => {
  const commit = "a".repeat(40);
  const { github, calls } = client([json({ id: 7, draft: true, target_commitish: commit, prerelease: false }),
    json({ object: { type: "commit", sha: "b".repeat(40) } })]);
  await assert.rejects(github.prepareDraft("app-v1.0.0", commit), /tag-commit-mismatch/);
  assert.ok(calls.every(call => call.method === "GET"));
});

test("asset replacement rechecks the release and refuses published assets", async () => {
  const { github, calls } = client([json({ draft: false })]);
  await assert.rejects(github.uploadAsset({ id: 7, draft: true }, "manifest.json", Buffer.from("{}")), /immutable-release/);
  assert.equal(calls.length, 1);
});

test("asset reads enforce declared and streamed size limits", async () => {
  const release = { assets: [{ id: 3, name: "manifest.json", state: "uploaded", size: 2 }] };
  const { github } = client([new Response("{}"), new Response("{}extra"), new Response("{")]);
  assert.equal((await github.downloadAsset(release, "manifest.json", 3)).toString(), "{}");
  await assert.rejects(github.downloadAsset(release, "manifest.json", 3), /too-large/);
  await assert.rejects(github.downloadAsset(release, "manifest.json", 3), /size-mismatch/);
  await assert.rejects(github.downloadAsset(release, "missing", 3), /missing/);
});
