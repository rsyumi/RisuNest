# Linux native persistence benchmark

This separate agent-only entry uses production persistence and regex adapters with synthetic fixtures. It does not render the full chat UI. Product builds never import it.

On an Ubuntu Linux development environment with Tauri dependencies, `WebKitWebDriver`, `xvfb`, and `dbus-run-session` installed:

```sh
pnpm tauri build --target x86_64-unknown-linux-gnu --no-bundle --config benchmarks/linux/tauri.conf.json
dbus-run-session -- xvfb-run -a python3 benchmarks/linux/run.py --binary "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/risunest" --output .tmp/linux-benchmark --phase persistence
dbus-run-session -- xvfb-run -a python3 benchmarks/linux/run.py --binary "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/risunest" --output .tmp/linux-benchmark --phase reload
dbus-run-session -- xvfb-run -a python3 benchmarks/linux/run.py --binary "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/risunest" --output .tmp/linux-benchmark --phase regex
node benchmarks/linux/summarize.mjs .tmp/linux-benchmark
```

Set `CARGO_TARGET_DIR` to the project's existing shared target. On the Windows/WSL development host that is `/mnt/e/Programming/Github/RisuNest/src-tauri/target`. Do not create another target directory. The Tauri CLI may use the configured `mainBinaryName`; pass the actual built executable path. Windows may build the frontend with the repository's local Vite entry, then WSL may build Cargo with the equivalent `TAURI_CONFIG` override.

The runner requires the benchmark identifier and verifies the native app-data directory before executing any workload. It creates its own `/var/tmp` XDG profile and saves its location in the output directory. Reuse the same output directory for the restart check. Never point this runner at an installed app or copy real user data into its profile.

Persistence uses 1 MiB and 6 MiB Unicode messages, two warmups per mode, then twenty alternating samples per mode. Each mutation checks the complete saved text and one revision increment. Replaying an already committed revision must fail. The reload phase checks the SHA-256 and revision after a new app process starts. The regex phase compares 500 safe rules against a JS oracle, including two warmups and twenty measured samples.

Results contain total save latency, frame intervals around a simple animated element, and sampled PSS/USS for the WebDriver child process tree. Memory is sampled every 200 ms and is an aggregate run observation, not an allocation trace or per-mode comparison. Windows shared buffers are not available on Linux; the optimized route is selected by the actual production platform adapter.

The routing hint counts string code units, not the final UTF-8 envelope size. A 1 MiB Unicode fixture may stay on JSON. Consult each sample's `commands` field before describing a result as raw IPC. The runner rejects missing memory samples instead of treating them as zero usage.

Xvfb/WSL results establish functional and relative diagnostic evidence. They do not certify physical Linux GPU, Wayland, IME, full chat rendering, cold start, or distribution package compatibility. Preserve sample distributions and do not substitute these values for those release gates.
