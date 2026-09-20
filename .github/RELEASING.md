# Releasing RisuNest

Stable releases are published automatically when a matching tag is pushed:

- App: `app-v<version>`
- Sync: `sync-v<version>`

Use a stable SemVer version and tag a reviewed commit contained in `main`.

## Prepare the release commit

For an App release, set the same version in:

- `version.json`
- `src-tauri/tauri.conf.json`
- `release-notes/app/<version>.md`

For a Sync release, set the same version in:

- `server/version.json`
- `server/sync/Cargo.toml`
- `server/manager/Cargo.toml`
- `server/manager/gui/package.json`
- `server/manager/gui/src-tauri/Cargo.toml`
- `server/manager/gui/src-tauri/tauri.conf.json`
- `release-notes/sync/<version>.md`

Update every affected lockfile in the same commit. Release notes must begin with a
Markdown title.

## Repository configuration

The release workflows use these repository Actions Secrets:

- `TAURI_PRIVATE_KEY`
- `TAURI_KEY_PASSWORD` (optional for an unencrypted update key)
- `RISUNEST_UPDATE_PUBLIC_KEY`
- `ANDROID_KEYSTORE_BASE64` (App only)
- `ANDROID_KEYSTORE_PASSWORD` (App only)
- `ANDROID_KEY_ALIAS` (App only)
- `ANDROID_KEY_PASSWORD` (App only)
- `RISUNEST_DEFAULT_REGISTRY_URL` (Sync only)

Keep the update signing identity and Android signing identity stable across
releases. Never commit private keys, passwords, or keystores.

`RISUNEST_RELEASE_BOOTSTRAP` is a repository Actions Variable. Keep it `false`
except when publishing the first product into an empty release catalogue. Set it
back to `false` immediately after that first publication succeeds.

## Publish

Create the tag from the prepared release commit and push that exact tag:

```powershell
$Sha = git rev-parse HEAD
git tag app-v1.0.0 $Sha
git push origin refs/tags/app-v1.0.0
```

For Sync, use the corresponding Sync tag:

```powershell
$Sha = git rev-parse HEAD
git tag sync-v0.1.0 $Sha
git push origin refs/tags/sync-v0.1.0
```

The tag version must match the product version files and release-notes filename.
If both products are released from the same commit, create both tags on that
commit and push both explicit tag refs together.

## Published releases are immutable

Do not move or reuse a published tag. Do not replace assets on a published
release. Publish a new version instead.
