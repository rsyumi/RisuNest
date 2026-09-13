# Mac desktop verification

This harness runs actual WKWebView and the product's Rust setup, command router,
storage and Mac event handler. It has its own executable, frontend and application
identifier (`io.github.rsyumi.risunest.macos.bench`). No verification commands,
fixtures or controllers are imported by the product entry.

Use a fresh CI user profile. The controller refuses an existing harness data root.
All data comes from the committed synthetic persistence/tokenizer/regex fixtures.
It never launches the installed product or reads its data.

```sh
pnpm install --frozen-lockfile
pnpm build:agent
pnpm benchmark:macos:build:agent
node benchmarks/macos/prepare.mjs
export CARGO_TARGET_DIR="$PWD/src-tauri/target"
export APPLE_SIGNING_IDENTITY=-
(cd benchmarks/macos/native && pnpm exec tauri build --ci --bundles app -- --locked)
python3 benchmarks/macos/run.py \
  --app "$CARGO_TARGET_DIR/release/bundle/macos/RisuNest Mac Bench.app" \
  --artifacts artifacts/wkwebview
```

On the shared Windows development checkout, always use the project-mandated
`E:/Programming/Github/RisuNest/src-tauri/target`, including from worktrees.

`prepare.mjs` derives the harness configuration and capabilities from the product.
Those generated files are ignored. The separate Cargo lock retains the product's
dependency versions. The harness adds only its own package, with no automation
server dependency or remote control listener.

The controller runs three process phases:

1. Contracts: JSON/Worker saves, malformed/stale rejection, exact Unicode/revision
   readback, native regex/JS oracle, literal tokenizer IDs/errors and batched
   native/WASM timing. Reload retains the committed revision and hash. Real
   LaunchServices actions reopen the hidden window and deliver Finder file URLs.
   A failed save cancels quit, then a successful save/checkpoint permits quit.
2. Restart: the same synthetic profile must retain the final revision and hash.
3. App: mount the ordinary Svelte product entry, await the native runtime and
   rendered UI, then exit through the product bootstrap's lifecycle listener.

Reports contain synthetic assertions, timings and hashes. Persistence samples
record the commands actually used, so small inputs remaining on JSON cannot be
mistaken for raw-IPC measurements. Warmups are excluded and measured orders
alternate. Frame gaps on a hosted VM are observations, not physical-device FPS.

Memory artifacts separate the app process tree's RSS from system-wide WebKit
service RSS, with a prelaunch baseline. launchd-owned WebKit processes cannot be
reliably attributed with `ps`; these values are not private footprint or a claim
of total memory savings. Physical-device responsiveness, power, dialogs and
distribution signing/notarization require their own validation.
