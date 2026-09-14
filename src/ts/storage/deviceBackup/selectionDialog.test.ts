import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { IDBFactory } from "fake-indexeddb";
import { tick } from "svelte";
import { language } from "../../../lang";
import {
  selectPortableBackupExport,
  selectPortableBackupRestore,
} from "./selectionDialog";

beforeEach(() => {
  vi.stubGlobal("indexedDB", new IDBFactory());
  vi.spyOn(HTMLDialogElement.prototype, "showModal").mockImplementation(
    function () {
      this.open = true;
    },
  );
  vi.spyOn(HTMLDialogElement.prototype, "close").mockImplementation(
    function () {
      this.open = false;
      this.dispatchEvent(new Event("close"));
    },
  );
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});
async function dialog(): Promise<HTMLDialogElement> {
  await vi.waitFor(() =>
    expect(document.querySelector("dialog")?.open).toBe(true),
  );
  return document.querySelector("dialog")!;
}
function submit(current: HTMLDialogElement) {
  current
    .querySelector("form")!
    .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
}

describe("portable backup scope selection", () => {
  it("defaults export to library and plugin sections, leaving device settings optional", async () => {
    const pending = selectPortableBackupExport();
    const current = await dialog();
    expect(
      [
        ...current.querySelectorAll<HTMLInputElement>('input[type="checkbox"]'),
      ].map((input) => input.checked),
    ).toEqual([true, true, true, false]);
    submit(current);
    await expect(pending).resolves.toEqual({
      library: true,
      deviceSections: ["local-storage", "localforage"],
    });
    expect(document.querySelector("dialog")).toBeNull();
  });
  it("prevents selecting a damaged library while preserving included device sections", async () => {
    const pending = selectPortableBackupRestore({
      libraryIncluded: true,
      repairRequired: true,
      deviceSections: ["local-storage", "device-settings"],
    });
    const current = await dialog();
    const library = current.querySelector<HTMLInputElement>(
      'input[type="checkbox"]',
    )!;
    expect(library.disabled).toBe(true);
    expect(library.checked).toBe(false);
    submit(current);
    await expect(pending).resolves.toEqual({
      library: false,
      deviceSections: ["local-storage", "device-settings"],
    });
  });
  it("omits unavailable library and device areas and disables an empty selection", async () => {
    const pending = selectPortableBackupRestore({
      libraryIncluded: false,
      repairRequired: false,
      deviceSections: ["localforage"],
    });
    const current = await dialog();
    expect(current.querySelectorAll('input[type="checkbox"]')).toHaveLength(1);
    current.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click();
    await tick();
    expect(
      current.querySelector<HTMLButtonElement>('button[type="submit"]')!
        .disabled,
    ).toBe(true);
    current.dispatchEvent(new Event("cancel", { cancelable: true }));
    await expect(pending).resolves.toBeNull();
    expect(document.querySelector("dialog")).toBeNull();
  });
  it("describes a first-run restore as an import instead of an overwrite", async () => {
    const preview = {
      libraryIncluded: true,
      repairRequired: false,
      deviceSections: ["local-storage"],
    };
    const firstRun = selectPortableBackupRestore(preview, { firstRun: true });
    let current = await dialog();
    expect(current.textContent).toContain(
      language.portableBackup.helpRestoreFirstRun,
    );
    expect(current.textContent).not.toContain(
      language.portableBackup.helpRestore,
    );
    submit(current);
    await expect(firstRun).resolves.toEqual({
      library: true,
      deviceSections: ["local-storage"],
    });

    const settings = selectPortableBackupRestore(preview);
    current = await dialog();
    expect(current.textContent).toContain(language.portableBackup.helpRestore);
    submit(current);
    await settings;
  });
});
