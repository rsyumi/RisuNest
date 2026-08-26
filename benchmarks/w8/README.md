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
