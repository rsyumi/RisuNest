import { mount, unmount } from "svelte";
import { language } from "../../../lang";
import PortableBackupSelection from "../../../lib/Setting/PortableBackupSelection.svelte";
import type {
  NativeArchiveInventory,
  NativePortableRestorePreview,
  NativePortableSelection,
} from "../nativeFileJobs";
import type { DataHealthResult } from "../dataHealth";
import {
  defaultNativePortableExportChoices,
  defaultNativePortableRestoreChoices,
  type DeviceSectionChoice,
  type NativePortableDeviceSection,
} from "./selection";

function localize(
  choice: DeviceSectionChoice<NativePortableDeviceSection>,
): DeviceSectionChoice<NativePortableDeviceSection> {
  const text = language.portableBackup;
  return {
    ...choice,
    label:
      choice.sectionId === "hypa"
        ? language.risuNest.localData.hypaTitle
        : choice.sectionId === "local-plugins"
          ? language.risuNest.localData.pluginTitle
          : text.settings,
  };
}

let selectionOpen = false;
interface ArchiveDetail {
  diagnosis?: DataHealthResult;
  items?: NativeArchiveInventory;
}

function showSelection(
  mode: "export" | "restore",
  choices: DeviceSectionChoice<NativePortableDeviceSection>[],
  libraryIncluded: boolean,
  repairRequired = false,
  firstRun = false,
  detail: ArchiveDetail = {},
): Promise<NativePortableSelection | null> {
  if (selectionOpen)
    return Promise.reject(new Error("A backup selection is already open"));
  selectionOpen = true;
  return new Promise((resolve, reject) => {
    const target = document.createElement("div");
    document.body.append(target);
    let component: ReturnType<typeof mount>;
    let settled = false;
    const finish = (
      selection: NativePortableSelection | null,
      error?: unknown,
    ) => {
      if (settled) return;
      settled = true;
      queueMicrotask(() => {
        void (component ? unmount(component) : Promise.resolve()).finally(
          () => {
            target.remove();
            selectionOpen = false;
            if (error !== undefined) reject(error);
            else resolve(selection);
          },
        );
      });
    };
    try {
      component = mount(PortableBackupSelection, {
        target,
        props: {
          mode,
          choices: choices.map(localize),
          libraryIncluded,
          repairRequired,
          diagnosis: detail.diagnosis,
          items: detail.items,
          firstRun,
          onDone: (selection) => finish(selection),
          onError: (error) => finish(null, error),
        },
      });
    } catch (error) {
      finish(null, error);
    }
  });
}

export async function selectPortableBackupExport(): Promise<NativePortableSelection | null> {
  const choices = defaultNativePortableExportChoices();
  return showSelection("export", choices, true);
}

export function selectPortableBackupRestore(
  preview: NativePortableRestorePreview,
  options: { firstRun?: boolean } = {},
): Promise<NativePortableSelection | null> {
  const choices = defaultNativePortableRestoreChoices(preview.deviceSections);
  return showSelection(
    "restore",
    choices,
    preview.libraryIncluded,
    preview.repairRequired,
    options.firstRun ?? false,
    { diagnosis: preview.diagnosis, items: preview.items },
  );
}
