import "./ts/polyfill";
import "core-js/actual";
import "./ts/consoleLogStartup";
import "katex/dist/katex.min.css";
import "./ts/storage/deviceSettingsStartup";
import "./ts/storage/database.svelte";
import App from "./App.svelte";
import { installAppLockInput } from "./ts/appLock/input";
import { initializeAppLock } from "./ts/appLock/appLock.svelte";
import { invoke } from "@tauri-apps/api/core";
import { isTauri } from "./ts/platform";
import { loadData } from "./ts/bootstrap";
import { initHotkey } from "./ts/hotkey";
import { preLoadCheck } from "./preload";
import { mount } from "svelte";
import { yieldToUi } from "./ts/ui/yieldToUi";
import { decideBoot } from "./ts/storage/recoveryMode.svelte";
import { recoveryStart } from "./ts/stores.svelte";
import { changeLanguage } from "./lang";
import { uiLanguageFor } from "./lang/uiLanguages";
import { installNativeWebviewHandling } from "./ts/nativeWebviewStartup";

installAppLockInput();
installNativeWebviewHandling();
preLoadCheck();
// The library's language setting has not loaded, so the start follows the device's until it does.
changeLanguage(uiLanguageFor(navigator.language));
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
void decideBoot().then(async (mode) => {
  if (mode === "normal") startNormally();
  else {
    if (isTauri) {
      await initializeAppLock({
        recovery: true,
        read: () => invoke<string | null>("app_lock_read_record"),
        commit: (record) => invoke<void>("app_lock_write_record", { record }),
      });
    }
    recoveryStart.set(startNormally);
  }
});
export default app;
