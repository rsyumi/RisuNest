# W8 bundle gates

These scripts measure the Roadmap 14 Highlight and SortableJS bundle gates without contacting RisuRealm.

## Initial production graph

Build the ordinary production application with source maps and a Vite manifest, then measure the static entry closure. Dynamic imports are intentionally excluded.

```powershell
$env:VITE_DISABLE_REALM = 'true'
pnpm exec vite build --manifest .vite/manifest.json --outDir dist --sourcemap
node benchmarks/w8/bundle-gates.mjs --dist dist
```

The report includes raw and gzip bytes for every entry or module-preload file. Source maps also show whether Highlight or SortableJS code is present in the startup graph.

## Reproducible Highlight split candidate

`highlight-candidate.patch` is the exact build-only candidate used for the retained gate result. It moves Highlight core and language registration behind dynamic imports. It is evidence, not a proposed product change. Its SHA-256 is:

```text
e610cac9870d2a43cabd664b293ce2bd74fba171af4b1c5c241e1c8d69710a59
```

The patch is pinned to LF in `.gitattributes`, so the hash is stable on Windows. Apply it only in a detached worktree created from the commit that introduced the artifact:

```powershell
$w8CandidateCommit = (git log -1 --format=%H -- benchmarks/w8/highlight-candidate.patch).Trim()
$w8CandidateRoot = Join-Path ([IO.Path]::GetTempPath()) 'risunest-w8-highlight-candidate-repro'
if (Test-Path -LiteralPath $w8CandidateRoot) { throw "Candidate worktree already exists: $w8CandidateRoot" }

git worktree add --detach $w8CandidateRoot $w8CandidateCommit
$w8Patch = Join-Path $w8CandidateRoot 'benchmarks\w8\highlight-candidate.patch'
$w8PatchHash = (Get-FileHash -LiteralPath $w8Patch -Algorithm SHA256).Hash.ToLowerInvariant()
if ($w8PatchHash -ne 'e610cac9870d2a43cabd664b293ce2bd74fba171af4b1c5c241e1c8d69710a59') {
    throw "Unexpected Highlight candidate patch hash: $w8PatchHash"
}
git -C $w8CandidateRoot apply --check benchmarks/w8/highlight-candidate.patch
git -C $w8CandidateRoot apply benchmarks/w8/highlight-candidate.patch

Push-Location $w8CandidateRoot
try {
    $env:VITE_DISABLE_REALM = 'true'
    pnpm install --frozen-lockfile
    pnpm exec vite build --manifest .vite/manifest.json --outDir dist --sourcemap
    node benchmarks/w8/bundle-gates.mjs --dist dist

    pnpm exec vite build --config benchmarks/w8/vite.highlight.config.ts --outDir dist-w8-highlight --emptyOutDir
    node benchmarks/w8/highlight-first-use.mjs --dist dist-w8-highlight --samples 20
} finally {
    Pop-Location
}
```

After recording the output, remove only that exact detached worktree:

```powershell
git worktree remove --force $w8CandidateRoot
```

## First Highlight use

Build the production-minified benchmark entry into a temporary directory and run it in installed Edge or Chrome. The runner disables the browser cache, reloads a fresh JavaScript realm for each sample, parses a JavaScript code block through the real `ParseMarkdown` function, and verifies highlighted output.

```powershell
$env:VITE_DISABLE_REALM = 'true'
$w8Output = Join-Path $env:TEMP 'risunest-w8-highlight-measurement'
pnpm exec vite build --config benchmarks/w8/vite.highlight.config.ts --outDir $w8Output --emptyOutDir
node benchmarks/w8/highlight-first-use.mjs --dist $w8Output --samples 20
```

Set `RISUNEST_W8_BROWSER` to an explicit Edge or Chrome executable when automatic discovery is not suitable.

## SortableJS removal upper bound

This build replaces every SortableJS import with a no-op build-only shim. It is not a runnable behavior candidate. The byte difference is the maximum initial graph saving available before a real lazy loader adds its own runtime code.

```powershell
$env:VITE_DISABLE_REALM = 'true'
pnpm exec vite build --config benchmarks/w8/vite.sortable-upper-bound.config.ts --manifest .vite/manifest.json --outDir dist --sourcemap
node benchmarks/w8/bundle-gates.mjs --dist dist
```

Do not use the upper-bound build for application testing or distribution.
