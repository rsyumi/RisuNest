import "./ts/polyfill";
import "core-js/actual"
import "./ts/storage/deviceSettingsStartup"
import "./ts/storage/database.svelte"
import App from "./App.svelte";
import { loadData } from "./ts/bootstrap";
import { initHotkey } from "./ts/hotkey";
import { preLoadCheck } from "./preload";
import { mount } from "svelte";

if (import.meta.env.VITE_TOKENIZER_BENCHMARK === 'true') {
    void import('./ts/tokenizer/nativeTokenizerBenchmark').then(({ installNativeTokenizerBenchmarkSeam }) => {
        installNativeTokenizerBenchmarkSeam()
    })
}

window.addEventListener('vite:preloadError', (event) => {
    console.error("Chunk load error detected:", event);
    alert("The server has been updated or the network connection has been lost. Please refresh the page.");
});

preLoadCheck()
let app = mount(App, {
    target: document.getElementById("app"),
});
loadData()
initHotkey()
document.getElementById('preloading').remove()

export default app;
