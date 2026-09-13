import { invoke } from "@tauri-apps/api/core";
import { appDataDir, documentDir, join } from "@tauri-apps/api/path";
import { mkdir, writeFile } from "@tauri-apps/plugin-fs";
import { exportIOSFile, pickIOSFile } from "../../src/ts/storage/iosFiles";
import {
  requestIOSNotifications,
  getIOSNativeState,
} from "../../src/ts/iosNative";

/** Installed only in the isolated synthetic UI-test app. */
export async function installPickerContracts() {
  const folder = await join(
    await appDataDir(),
    "ios-file-staging",
    crypto.randomUUID(),
  );
  await mkdir(folder, { recursive: true });
  const path = await join(folder, "synthetic.bin");
  await writeFile(path, Uint8Array.from([0, 1, 127, 128, 254, 255]));
  await mkdir(await join(await documentDir(), "Exports"), { recursive: true });
  const status = document.getElementById("status")!;
  const action = (name: string, work: () => Promise<string>) => {
    const button = document.createElement("button");
    button.textContent = name;
    button.style.cssText =
      "display:block;margin:20px;padding:20px;font-size:18px";
    button.onclick = async () => {
      status.textContent = "working";
      try {
        status.textContent = await work();
      } catch (error) {
        status.textContent =
          error instanceof DOMException && error.name === "AbortError"
            ? "export-cancelled"
            : "failed";
      }
      await invoke("ios_bench_report", {
        stage: "ui",
        result: { action: name, outcome: status.textContent },
      });
    };
    document.body.append(button);
  };
  action("Import synthetic file", async () =>
    (await pickIOSFile()) ? "imported" : "import-cancelled",
  );
  action("Export synthetic file", async () => {
    const result = await exportIOSFile({
      sourcePath: path,
      suggestedName: "synthetic.bin",
    });
    if (result.bytes !== 6) throw new Error("Incorrect publication length");
    return "exported";
  });
  action("Allow notifications", async () => {
    await requestIOSNotifications();
    return (await getIOSNativeState()).notifications
      ? "notifications-allowed"
      : "notifications-denied";
  });
  status.textContent = "ui-ready";
}
