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

window.addEventListener("vite:preloadError", (event) => {
  console.error("Chunk load error detected:", event);
  alert(
    "The server has been updated or the network connection has been lost. Please refresh the page.",
  );
});

preLoadCheck();
const app = mount(App, {
  target: document.getElementById("app"),
});
document.getElementById("preloading")?.remove();
void yieldToUi().then(() => {
  loadData();
  initHotkey();
});
export default app;
