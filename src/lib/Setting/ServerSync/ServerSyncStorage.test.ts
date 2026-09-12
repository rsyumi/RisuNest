import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, unmount } from "svelte";
import type { ServerSyncBackupInventory } from "src/ts/storage/sync/serverSyncProduction";
const native = vi.hoisted(() => ({
  getServerSyncBackupInventory: vi.fn(),
  getServerSyncCacheUsage: vi.fn(),
  cleanupServerSyncCache: vi.fn(),
  deleteServerSyncBackup: vi.fn(),
  restoreServerSyncBackup: vi.fn(),
}));
const confirm = vi.hoisted(() => vi.fn());
vi.mock("src/ts/storage/sync/serverSyncProduction", () => native);
vi.mock("src/ts/alert", () => ({ alertConfirm: confirm }));
vi.mock("src/lang", async () => ({
  language: (await import("src/lang/en")).languageEnglish,
}));
import ServerSyncStorage from "./ServerSyncStorage.svelte";
import { languageEnglish } from "src/lang/en";
const labels = languageEnglish.risuNest.serverSync.management;
const inventory = (): ServerSyncBackupInventory => ({
  items: [
    {
      id: "synthetic-id",
      createdAt: 1,
      localRevision: 2,
      head: {
        libraryId: "library",
        epoch: "epoch",
        seq: "1",
        headId: "head",
        minRetainedSeq: "0",
      },
      localBytes: 1024,
      remoteBytes: 2048,
      preservationScope: "library",
      recoveryReady: true,
      diskBytes: 3200,
      deletable: true,
      blockedReason: null,
    },
  ],
  next: { createdAt: 1, id: "synthetic-id" },
  completeCount: 105,
  completeBytes: 4096,
  incompleteCount: 1,
  incompleteBytes: 512,
  diskBytes: 5000,
});
let target: HTMLDivElement;
let component: ReturnType<typeof mount>;
let changed = vi.fn<() => void>();
const button = (label: string) =>
  [...target.querySelectorAll<HTMLButtonElement>("button")].find((b) =>
    b.textContent?.trim().startsWith(label),
  )!;
beforeEach(() => {
  vi.resetAllMocks();
  native.getServerSyncBackupInventory.mockResolvedValue(inventory());
  native.getServerSyncCacheUsage.mockResolvedValue({
    totalBytes: 2048,
    protectedBytes: 1024,
    reclaimableBytes: 1024,
    blockedReason: null,
  });
  confirm.mockResolvedValue(true);
  changed = vi.fn();
  target = document.createElement("div");
  document.body.append(target);
  component = mount(ServerSyncStorage, {
    target,
    props: { onChange: changed },
  });
});
afterEach(async () => {
  await unmount(component);
  target.remove();
});
describe("shared local server storage management", () => {
  it("displays whole-inventory totals separately from the current page and fetches older rows by cursor", async () => {
    await vi.waitFor(() => expect(button(labels.more)).toBeDefined());
    expect(target.textContent).toContain("(105)");
    expect(target.textContent).toContain("4.9 KiB");
    expect(target.textContent).toContain(labels.incomplete);
    button(labels.more).click();
    await vi.waitFor(() =>
      expect(native.getServerSyncBackupInventory).toHaveBeenLastCalledWith(
        inventory().next,
      ),
    );
  });
  it("requires confirmation and sends only the chosen native ID for deletion", async () => {
    await vi.waitFor(() =>
      expect(button(languageEnglish.remove)).toBeDefined(),
    );
    confirm.mockResolvedValueOnce(false);
    button(languageEnglish.remove).click();
    await vi.waitFor(() =>
      expect(confirm).toHaveBeenCalledWith(labels.deleteConfirm),
    );
    expect(native.deleteServerSyncBackup).not.toHaveBeenCalled();
    button(languageEnglish.remove).click();
    await vi.waitFor(() => expect(changed).toHaveBeenCalledOnce());
    expect(native.deleteServerSyncBackup).toHaveBeenCalledExactlyOnceWith(
      "synthetic-id",
    );
  });
  it("restores the chosen side through the existing guarded file route", async () => {
    await vi.waitFor(() =>
      expect(
        button(languageEnglish.risuNest.serverSync.restoreRemoteBackup),
      ).toBeDefined(),
    );
    button(languageEnglish.risuNest.serverSync.restoreRemoteBackup).click();
    await vi.waitFor(() =>
      expect(native.restoreServerSyncBackup).toHaveBeenCalledExactlyOnceWith(
        "synthetic-id",
        "remote",
      ),
    );
    expect(confirm).toHaveBeenCalledWith(labels.restoreConfirm);
  });
  it("rechecks listing after refresh and disables deletion, restoration and cache cleanup when native reports protection", async () => {
    await vi.waitFor(() => expect(button(labels.clean)).toBeDefined());
    const blocked = inventory();
    blocked.items[0] = {
      ...blocked.items[0],
      deletable: false,
      blockedReason: "backup-in-use",
    };
    native.getServerSyncBackupInventory.mockResolvedValue(blocked);
    native.getServerSyncCacheUsage.mockResolvedValue({
      totalBytes: 2048,
      protectedBytes: 2048,
      reclaimableBytes: 0,
      blockedReason: "resolve-pending-operation-first",
    });
    button(labels.refresh).click();
    await vi.waitFor(() =>
      expect(button(languageEnglish.remove).disabled).toBe(true),
    );
    expect(
      button(languageEnglish.risuNest.serverSync.restoreLocalBackup).disabled,
    ).toBe(true);
    expect(button(labels.clean).disabled).toBe(true);
  });
  it("hides stale usage after a partial cleanup failure, refreshes dashboard totals and never renders native detail", async () => {
    await vi.waitFor(() => expect(button(labels.clean)).toBeDefined());
    native.cleanupServerSyncCache.mockRejectedValue(
      new Error("synthetic private path and secret"),
    );
    button(labels.clean).click();
    await vi.waitFor(() => expect(changed).toHaveBeenCalledOnce());
    expect(confirm).toHaveBeenCalledWith(labels.cleanConfirm);
    expect(target.querySelector("dl")).toBeNull();
    expect(target.textContent).toContain(
      languageEnglish.risuNest.storage.actionFailed,
    );
    expect(target.textContent).not.toContain("synthetic private");
    button(labels.refresh).click();
    await vi.waitFor(() => expect(target.querySelector("dl")).not.toBeNull());
  });
});
