import "../../src/ts/polyfill";
import "core-js/actual";
import "katex/dist/katex.min.css";
import "../../src/ts/storage/deviceSettingsStartup";
import { startStreamingSmoke } from "./fixture";

startStreamingSmoke();
