import "./ts/polyfill";
import "core-js/actual";
import "katex/dist/katex.min.css";
import "./ts/storage/deviceSettingsStartup";
import "./ts/storage/database.svelte";
import App from "./App.svelte";
import { loadData } from "./ts/bootstrap";
import { initHotkey } from "./ts/hotkey";
import { preLoadCheck } from "./preload";
import { mount } from "svelte";
import { yieldToUi } from "./ts/ui/yieldToUi";
import { decideBoot } from "./ts/storage/recoveryMode.svelte";
import { recoveryStart } from "./ts/stores.svelte";

preLoadCheck();
const app = mount(App, {
  target: document.getElementById("app"),
});
document.getElementById("preloading")?.remove();

function startNormally(): void {
  void yieldToUi().then(() => {
    const loaded = loadData();
    initHotkey();
    void Promise.resolve(loaded).then(async () => {
      const { resumePortableExportsAfterBootstrap } = await import(
        "./ts/storage/deviceBackup/jobRecovery"
      );
      await resumePortableExportsAfterBootstrap();
    });
  });
}

// The recovery shell is the app without a library behind it, so `loadData()` waits until the
// boot decision says this start may run it.
void decideBoot().then((mode) => {
  if (mode === "normal") startNormally();
  else recoveryStart.set(startNormally);
});
export default app;
