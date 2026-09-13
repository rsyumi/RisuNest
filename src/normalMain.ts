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

preLoadCheck();
const app = mount(App, {
  target: document.getElementById("app"),
});
document.getElementById("preloading")?.remove();
void yieldToUi().then(() => {
  const loaded = loadData();
  initHotkey();
  void Promise.resolve(loaded).then(async () => {
    const { resumePortableExportsAfterBootstrap } =
      await import("./ts/storage/deviceBackup/jobRecovery");
    await resumePortableExportsAfterBootstrap();
  });
});
export default app;
