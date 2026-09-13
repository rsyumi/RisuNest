# Native media transport measurement

Only synthetic data and a fresh temporary WebView2 profile are used. No app bootstrap, installed library, or network endpoint is opened. The native probe is behind `cfg(test)` and an ignored test, so it does not enter the product runtime.

Build the native test binary with the shared Cargo target:

```powershell
$env:CARGO_TARGET_DIR='E:/Programming/Github/RisuNest/src-tauri/target'
cargo test --manifest-path src-tauri/Cargo.toml --offline --locked --lib native_media:: --no-run
```

Pass the emitted test executable to `windows-memory.ps1 -TestBinary <path> -Mode buffered -Trial 1`, then to the same script with `-Mode streaming`. The former recreates the previous full-body Wry response; the latter uses the actual product loopback server. The probe loads a synthetic 4096×4096 32-bit BMP (64MiB plus its header), validates image dimensions and a canvas pixel, and reports image load time. This is an isolated debug transport experiment, not a release-app UI benchmark.

The script records native private commit and the sum of private commit for the test process and its WebView2 descendant processes. It requests a 20ms sleep between samples; actual sample intervals are longer due to Windows process queries and must be read from the recorded timestamps. Peaks are sampled lower bounds, not exact high-water marks. Private commit is not Android PSS, unique resident memory, or GPU allocation.

Results and individual samples are written under `.tmp/media-memory/`. Repeat with distinct trial numbers. Do not run another Cargo link of the same test executable while a probe is active.
