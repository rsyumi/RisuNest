// No normal app imports are allowed before the native device recovery gate.
// Native navigation and writer checks precede every normal app initialization.
import { deviceMaintenanceBeforeBootstrap } from "./ts/storage/deviceBackup/entry";
import { appCleanupBeforeBootstrap } from "./ts/storage/appCleanupEntry";
import { createStartupSurface } from "./ts/appLock/startupSurface";

window.addEventListener("vite:preloadError", (event) => {
  console.error("Chunk load error detected:", event);
  alert(
    "The server has been updated or the network connection has been lost. Please refresh the page.",
  );
});

// A startup panel waits without mounting the app, so it takes over the native launch cover itself.
const startupSurface = createStartupSurface();
const onPanelShown = () => void startupSurface.present();
const app = appCleanupBeforeBootstrap({ onPanelShown }).then(() => deviceMaintenanceBeforeBootstrap({ onPanelShown })).then(
  () => import("./normalMain"),
);
export default app;
