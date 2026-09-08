# Synthetic UI responsiveness benchmark

This benchmark mounts the production `Chats.svelte` and `ChatBody.svelte` path with two deterministic in-memory characters. Each character contains 180 synthetic markdown messages. It does not import the app bootstrap, open IndexedDB, or read a backup.

The runner starts a headless Edge or Chrome process with a fresh temporary profile, enables the repository Realm URL blocklist before the first benchmark navigation, disables browser caching, and rejects any HTTP request outside the loopback server. It verifies every one of the 64 mounted viewport rows has the expected index and marker, a parsed code block, and a parsed table. It also hashes all normalized mounted text and requires matching signatures across samples. Output contains timing numbers and a fixture hash, never chat content or application URLs.

`moduleLoadToFirstChatReadyDuration` includes benchmark module load, evaluation, and the first verified 64-row viewport. It is not the full application or database boot time. `firstChatDuration` isolates the Svelte mount and render portion. `navigationDuration` measures switching to the second synthetic character through the same production component. `totalReadyDuration` ends after both first render and navigation are verified.

Run at least three baseline samples against the pinned source revision:

```powershell
node benchmarks/ui-responsiveness/runner.mjs --source baseline --samples 5
```

Run the same fixture against the working tree:

```powershell
node benchmarks/ui-responsiveness/runner.mjs --source candidate --samples 5
```

Set `RISUNEST_UI_BENCH_BROWSER` to an Edge or Chrome executable when automatic discovery is unsuitable. The baseline Vite plugin overlays changed `src/` files from revision `793930f17731547c7e9f2fe7bf0be6f6a5bbd462` using read-only Git commands, so baseline and candidate builds share the current harness and differ only in product source.

Add `--screenshot` to either command to write `synthetic-layout.png` beside this README for optional visual QA. The screenshot contains only the local synthetic fixture.

Run the separate navigation-overlay visual check with:

```powershell
node benchmarks/ui-responsiveness/visual-runner.mjs
```

It mounts the production `ChatScreen.svelte` overlay and `LoadingIndicator.svelte` around a small synthetic chat-content stub. It writes dated desktop and narrow-pane screenshots under `docs/research`, verifies the overlay stays within the chat pane, checks that the spinner is centered, and confirms a pointer click cannot activate the inert content beneath it. This is visual layout QA only. It does not mount the full app shell, native window, mobile shell, or database bootstrap.
