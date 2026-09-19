import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import test from "node:test";

const root = new URL("../../", import.meta.url);
const workflows = new URL(".github/workflows/", root);

test("unsupported container and Cloudflare Pages distribution channels stay removed", () => {
  for (const path of [
    ".dockerignore",
    "Dockerfile",
    "docker-compose.yml",
    ".github/workflows/docker-build.yml",
    ".github/workflows/nightly-deploy.yml",
  ]) assert.equal(existsSync(new URL(path, root)), false, `${path} must stay removed`);

  const workflowContents = readdirSync(workflows, { withFileTypes: true })
    .filter((entry) => entry.isFile() && /\.ya?ml$/.test(entry.name))
    .map((entry) => readFileSync(new URL(entry.name, workflows), "utf8"))
    .join("\n");
  assert.doesNotMatch(workflowContents, /docker\/(?:setup-buildx|login|metadata|build-push)-action|docker buildx build|ghcr\.io/);
  assert.doesNotMatch(workflowContents, /cloudflare\/wrangler-action|pages deploy|CLOUDFLARE_(?:API_TOKEN|ACCOUNT_ID|NIGHTLY_PROJECT_NAME)/);

  const readme = readFileSync(new URL("README.md", root), "utf8");
  assert.doesNotMatch(readme, /Docker Installation|docker compose|run RisuNest using Docker/i);
});

test("pull requests run only the standard and CodeQL checks", () => {
  const pullRequestWorkflows = readdirSync(workflows, { withFileTypes: true })
    .filter((entry) => entry.isFile() && /\.ya?ml$/.test(entry.name))
    .filter((entry) => /^  pull_request:/m.test(readFileSync(new URL(entry.name, workflows), "utf8")))
    .map((entry) => entry.name)
    .sort();
  assert.deepEqual(pullRequestWorkflows, ["codeql.yml", "pr-check.yml"]);

  const pullRequestCheck = readFileSync(new URL("pr-check.yml", workflows), "utf8");
  assert.doesNotMatch(pullRequestCheck, /pull-requests:\s*write/);
});

test("CodeQL analyzes the native Rust sources without a product build", () => {
  const codeql = readFileSync(new URL("codeql.yml", workflows), "utf8").replace(/\r\n/g, "\n");
  assert.match(codeql, /- language: rust\n\s+build-mode: none/);
});
