import { REPOSITORY, parseTag } from "./contracts.mjs";

export class GitHubReleases {
  constructor(token, fetcher = fetch) {
    if (!token) throw new Error("github-token-required");
    this.token = token;
    this.fetcher = fetcher;
  }

  async request(path, { method = "GET", body, binary = false, allow404 = false, upload = false } = {}) {
    const host = upload ? "https://uploads.github.com" : "https://api.github.com";
    const response = await this.fetcher(`${host}/repos/${REPOSITORY}${path}`, {
      method, signal: AbortSignal.timeout(upload ? 10 * 60 * 1000 : 60000),
      headers: { Authorization: `Bearer ${this.token}`, "X-GitHub-Api-Version": "2022-11-28",
        Accept: binary ? "application/octet-stream" : "application/vnd.github+json",
        ...(body === undefined ? {} : { "Content-Type": upload ? "application/octet-stream" : "application/json" }) },
      body: body === undefined ? undefined : upload ? body : JSON.stringify(body),
    });
    if (allow404 && response.status === 404) return null;
    if (!response.ok) throw new Error(`github-${method.toLowerCase()}-${response.status}`);
    if (response.status === 204) return null;
    return binary ? response : response.json();
  }

  getByTag(tag) { parseTag(tag); return this.request(`/releases/tags/${encodeURIComponent(tag)}`, { allow404: true }); }
  getLatest() { return this.request("/releases/latest", { allow404: true }); }
  getById(id) { return this.request(`/releases/${Number(id)}`); }
  async hasPublishedStable() {
    for (let page = 1; ; page++) {
      const releases = await this.request(`/releases?per_page=100&page=${page}`);
      if (releases.some(release => !release.draft && !release.prerelease)) return true;
      if (releases.length < 100) return false;
    }
  }

  async listPublishedStable() {
    const stable = [];
    for (let page = 1; ; page++) {
      const releases = await this.request(`/releases?per_page=100&page=${page}`);
      stable.push(...releases.filter(release => !release.draft && !release.prerelease));
      if (releases.length < 100) return stable;
    }
  }

  async downloadAsset(release, name, limit) {
    const asset = release.assets?.find(asset => asset.name === name);
    if (!asset || asset.state !== "uploaded" || asset.size > limit) throw new Error("missing-or-oversized-release-asset");
    const response = await this.request(`/releases/assets/${asset.id}`, { binary: true });
    const chunks = [];
    let length = 0;
    for await (const chunk of response.body) {
      length += chunk.length;
      if (length > limit) throw new Error("release-asset-too-large");
      chunks.push(chunk);
    }
    if (length !== asset.size) throw new Error("release-asset-size-mismatch");
    return Buffer.concat(chunks);
  }

  async uploadAsset(release, name, bytes) {
    const fresh = await this.getById(release.id);
    if (!fresh.draft) throw new Error("immutable-release");
    const existing = fresh.assets.find(asset => asset.name === name);
    if (existing) await this.request(`/releases/assets/${existing.id}`, { method: "DELETE" });
    await this.request(`/releases/${release.id}/assets?name=${encodeURIComponent(name)}`, { method: "POST", body: bytes, upload: true });
  }

  publish(id) { return this.request(`/releases/${Number(id)}`, { method: "PATCH", body: { draft: false, make_latest: "true" } }); }
  makeLatest(id) { return this.request(`/releases/${Number(id)}`, { method: "PATCH", body: { make_latest: "true" } }); }

  async assertTagCommit(tag, commit) {
    parseTag(tag);
    if (!/^[a-f0-9]{40}$/.test(commit)) throw new Error("invalid-source-commit");
    // A tag must already exist and resolve to the tested commit; creating tags is not this job's responsibility.
    let ref = await this.request(`/git/ref/tags/${encodeURIComponent(tag)}`);
    for (let i = 0; ref.object.type === "tag" && i < 5; i++) {
      ref = await this.request(`/git/tags/${ref.object.sha}`);
    }
    if (ref.object.type !== "commit" || ref.object.sha !== commit) throw new Error("tag-commit-mismatch");
  }

  async prepareDraft(tag, commit, body = "") {
    parseTag(tag);
    if (!/^[a-f0-9]{40}$/.test(commit)) throw new Error("invalid-source-commit");
    const previous = await this.getByTag(tag);
    if (previous && (previous.target_commitish !== commit || previous.prerelease)) throw new Error("release-identity-conflict");
    await this.assertTagCommit(tag, commit);
    if (previous) return previous;
    return this.request("/releases", { method: "POST", body: { tag_name: tag,
      target_commitish: commit, name: tag, body, draft: true, prerelease: false, make_latest: "false" } });
  }
}
