import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { IDBFactory } from "fake-indexeddb";
import { tick } from "svelte";
import { language } from "../../../lang";
import {
  selectPortableBackupExport,
  selectPortableBackupRestore,
} from "./selectionDialog";
import type { NativePortableRestorePreview } from "../nativeFileJobs";

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
      deviceSections: ["hypa", "local-plugins"],
    });
    expect(document.querySelector("dialog")).toBeNull();
  });
  it("prevents selecting a damaged library while preserving included device sections", async () => {
    const pending = selectPortableBackupRestore({
      libraryIncluded: true,
      repairRequired: true,
      deviceSections: ["hypa", "local-settings"],
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
      deviceSections: ["hypa", "local-settings"],
    });
  });
  it("omits unavailable library and device areas and disables an empty selection", async () => {
    const pending = selectPortableBackupRestore({
      libraryIncluded: false,
      repairRequired: false,
      deviceSections: ["local-plugins"],
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
    const preview: NativePortableRestorePreview = {
      libraryIncluded: true,
      repairRequired: false,
      deviceSections: ["hypa"],
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
      deviceSections: ["hypa"],
    });

    const settings = selectPortableBackupRestore(preview);
    current = await dialog();
    expect(current.textContent).toContain(language.portableBackup.helpRestore);
    submit(current);
    await settings;
  });

  it("keeps a present empty native section selected for explicit restore", async () => {
    const pending = selectPortableBackupRestore({
      libraryIncluded: false,
      repairRequired: false,
      deviceSections: ["local-settings"],
    });
    const current = await dialog();
    expect(current.textContent).toContain(language.portableBackup.settings);
    expect(
      current.querySelector<HTMLInputElement>('input[type="checkbox"]')!
        .checked,
    ).toBe(true);
    submit(current);
    await expect(pending).resolves.toEqual({
      library: false,
      deviceSections: ["local-settings"],
    });
  });

  it("rejects duplicate and browser-only section identifiers", () => {
    expect(() =>
      selectPortableBackupRestore({
        libraryIncluded: true,
        repairRequired: false,
        deviceSections: ["hypa", "hypa"],
      }),
    ).toThrow("Duplicate native portable device section");
    expect(() =>
      selectPortableBackupRestore({
        libraryIncluded: true,
        repairRequired: false,
        deviceSections: ["local-storage" as never],
      }),
    ).toThrow("Invalid native portable device section");
  });
});

describe("a damaged archive offers what is still whole", () => {
    const damaged = {
        libraryIncluded: true,
        repairRequired: true,
        deviceSections: [],
        diagnosis: {
            revision: 0,
            scannedAt: 1,
            depth: "quick" as const,
            counts: { blocking: 1, degraded: 1, informational: 0 },
            items: [],
            omitted: 0,
        },
        items: {
            characters: [
                { id: "char-a", conversations: 2, damaged: 0 },
                { id: "char-b", conversations: 0, damaged: 3 },
            ],
            presets: [{ id: "0", conversations: 0, damaged: 0 }],
            plugins: [],
        },
    };

    it("says what is wrong and preselects only the records that are whole", async () => {
        const pending = selectPortableBackupRestore(damaged);
        const current = await dialog();
        expect(
            current.querySelector("[data-portable-diagnosis]")?.textContent,
        ).toContain(language.portableBackup.damaged.replace("{0}", "2"));
        expect(
            current.querySelector("[data-portable-items='characters']")
                ?.textContent,
        ).toContain(language.portableBackup.itemDamaged.replace("{0}", "3"));

        submit(current);
        await expect(pending).resolves.toEqual({
            library: true,
            deviceSections: [],
            items: {
                characters: ["char-a"],
                presets: ["0"],
                plugins: [],
                excluded: ["char-b"],
            },
        });
    });

    it("brings in what the reader adds back, and leaves out what they clear", async () => {
        const pending = selectPortableBackupRestore(damaged);
        const current = await dialog();
        const boxes = [
            ...current.querySelectorAll<HTMLInputElement>(
                "[data-portable-items] input[type='checkbox']",
            ),
        ];
        // The damaged character is added back, and the preset is cleared.
        boxes[1].click();
        boxes[2].click();
        await tick();
        submit(current);
        await expect(pending).resolves.toEqual({
            library: true,
            deviceSections: [],
            items: {
                characters: ["char-a", "char-b"],
                presets: [],
                plugins: [],
                excluded: ["0"],
            },
        });
    });

    it("keeps the whole-library choice unavailable while the archive is refused", async () => {
        const pending = selectPortableBackupRestore(damaged);
        const current = await dialog();
        const library = [...current.querySelectorAll("label")]
            .find((label) => label.textContent?.includes(language.portableBackup.library))
            ?.querySelector<HTMLInputElement>("input[type='checkbox']");
        expect(library?.disabled).toBe(true);
        expect(library?.checked).toBe(false);
        submit(current);
        await pending;
    });
})
