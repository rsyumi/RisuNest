import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { documentDir, join } from "@tauri-apps/api/path";
import { mkdir, readFile, writeFile } from "@tauri-apps/plugin-fs";
import {
  discardIOSFile,
  exportIOSFile,
  getIOSPublication,
  pickIOSFile,
} from "../../src/ts/storage/iosFiles";
import {
  requestIOSNotifications,
  getIOSNativeState,
} from "../../src/ts/iosNative";
import { nativeDataPath } from "../../src/ts/storage/nativePaths";
import { selectDomFiles } from "../../src/ts/storage/fileSelection";
import { withNativeSelectedFile } from "../../src/ts/storage/nativeFileSelection";
import { failureRecord, type FailureRecord } from "../failureRecord";

/** Installed only in the isolated synthetic UI-test app. */
export async function installPickerContracts(generalOnly = false) {
  const folder = await nativeDataPath("ios-file-staging", crypto.randomUUID());
  await mkdir(folder, { recursive: true });
  const path = await join(folder, "synthetic.bin");
  await writeFile(path, Uint8Array.from([0, 1, 127, 128, 254, 255]));
  if (generalOnly) {
    await writeFile(await join(await documentDir(), "native-general.risup"), Uint8Array.from([0, 1, 127, 128, 254, 255]));
  }
  const status = document.getElementById("status")!;
  const action = (name: string, work: () => Promise<string>) => {
    const button = document.createElement("button");
    button.textContent = name;
    button.style.cssText =
      "display:block;margin:20px;padding:20px;font-size:18px";
    button.onclick = async () => {
      status.textContent = "working";
      let errorDetail: string | undefined;
      let failure: FailureRecord | undefined;
      try {
        status.textContent = await work();
      } catch (error) {
        errorDetail = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
        failure = failureRecord(error);
        status.textContent =
          error instanceof DOMException && error.name === "AbortError"
            ? "export-cancelled"
            : "failed";
      }
      await invoke("ios_bench_report", {
        stage: "ui",
        result: { action: name, outcome: status.textContent, error: errorDetail, failure },
      });
    };
    document.body.append(button);
  };
  if (generalOnly) {
    action("General single synthetic file", async () => {
      const [file] = await selectDomFiles({ accept: ".risup" });
      if (!file) return "general-single-cancelled";
      const bytes = new Uint8Array(await file.arrayBuffer());
      const expected = [0, 1, 127, 128, 254, 255];
      if (file.name !== "native-general.risup" || bytes.length !== expected.length ||
          !bytes.every((byte, index) => byte === expected[index])) {
        throw new Error("Incorrect general picker file name or binary content");
      }
      return "general-single-exact";
    });
    action("General sliced synthetic file", async () => {
      const result = await withNativeSelectedFile({ accept: ".risup" }, async file => {
        const bytes = new Uint8Array(await file.slice(2, 5).arrayBuffer());
        if (file.name !== "native-general.risup" || file.size !== 6 ||
            bytes.length !== 3 || bytes[0] !== 127 || bytes[1] !== 128 || bytes[2] !== 254) {
          throw new Error("Incorrect native slice content");
        }
        return "general-slice-exact";
      });
      return result ?? "general-slice-cancelled";
    });
    action("General multiple synthetic files", async () => {
      const files = await selectDomFiles({ accept: ".risup", multiple: true });
      return files.length ? "general-multiple-selected" : "general-multiple-cancelled";
    });
    status.textContent = "ui-ready";
    return;
  }
  action("Import synthetic file", async () => {
    const picked = await pickIOSFile();
    if (!picked) return "import-cancelled";
    try {
      const actual = await readFile(picked.path);
      const expected = [0, 1, 127, 128, 254, 255];
      if (
        picked.bytes !== expected.length ||
        actual.length !== expected.length ||
        !actual.every((byte, index) => byte === expected[index])
      ) {
        throw new Error("Incorrect imported binary content");
      }
      return "imported-exact";
    } finally {
      await discardIOSFile(picked.path);
    }
  });
  action("Export synthetic file", async () => {
    const requestId = crypto.randomUUID();
    const result = await exportIOSFile({
      sourcePath: path,
      suggestedName: "synthetic.bin",
      requestId,
    });
    if (result.bytes !== 6) throw new Error("Incorrect publication length");
    if ((await getIOSPublication(requestId))?.bytes !== 6)
      throw new Error("Missing durable publication receipt");
    return "exported";
  });
  action("Allow notifications", async () => {
    await requestIOSNotifications();
    return (await getIOSNativeState()).notifications
      ? "notifications-allowed"
      : "notifications-denied";
  });
  action("Open synthetic link", async () => {
    await openUrl("https://ios-synthetic.invalid/");
    return "link-opened";
  });
  status.textContent = "ui-ready";
}
