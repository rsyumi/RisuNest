import { readFileSync, existsSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
const source = (file: string) => readFileSync(resolve(file), "utf8");
describe("server-only product boundaries", () => {
  it("has no peer engine, native schema, or Android transport files", () => {
    for (const file of [
      "src-tauri/src/peer_sync",
      "src-tauri/src/persistent_store/logical_schema.rs",
      "src-tauri/src/persistent_store/logical_delta_source.rs",
      "src-tauri/src/persistent_store/logical_delta_target.rs",
      "src-tauri/src/persistent_store/sync_device_registry.rs",
      "src/ts/storage/sync/deviceSyncController.ts",
      "src/ts/storage/sync/peerClone.ts",
      "src/lib/Setting/Pages/DeviceSyncSettings.svelte",
      "src-tauri/gen/android/app/src/main/java/io/github/rsyumi/risunest/PeerCloneTransfer.kt",
      "src-tauri/gen/android/app/src/main/java/io/github/rsyumi/risunest/PeerSyncForegroundService.kt",
    ])
      expect(existsSync(resolve(file))).toBe(false);
  });
  it("keeps peer calls out of startup, UI, native registration, and Android bridges", () => {
    for (const file of [
      "src/App.svelte",
      "src/ts/bootstrap.ts",
      "src/ts/characterCards.ts",
      "src/lib/Others/Onboarding/Onboarding.svelte",
      "src/lib/Setting/Settings.svelte",
      "src-tauri/src/lib.rs",
      "src-tauri/src/persistent_store/commands.rs",
      "src-tauri/gen/android/app/src/main/AndroidManifest.xml",
      "src-tauri/gen/android/app/src/main/java/io/github/rsyumi/risunest/MainActivity.kt",
    ])
      expect(source(file)).not.toMatch(
        /peer_sync|peer_clone|peer_delta|peer_bidirectional|device_sync|DeviceSync|PeerClone|PeerSync|PEER_CLONE|sync-device/,
      );
  });
  it("preserves upstream multiuser, generation keep-alive, SAF and OS deep-link registration", () => {
    expect(source("src/ts/sync/multiuser.ts")).toContain("import('peerjs')");
    const activity = source(
      "src-tauri/gen/android/app/src/main/java/io/github/rsyumi/risunest/MainActivity.kt",
    );
    expect(activity).toContain("GenerationKeepAliveBridge");
    expect(activity).toContain("Saf");
    const manifest = source(
      "src-tauri/gen/android/app/src/main/AndroidManifest.xml",
    );
    expect(manifest).toContain(".GenerationForegroundService");
    expect(manifest).toContain('android:scheme="risunestlocal"');
    expect(manifest).toContain(
      "android.permission.FOREGROUND_SERVICE_DATA_SYNC",
    );
  });
});
