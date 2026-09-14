import { mount, unmount } from "svelte";
import { language } from "../../../lang";
import PortableBackupSelection from "../../../lib/Setting/PortableBackupSelection.svelte";
import type {
  NativePortableRestorePreview,
  NativePortableSelection,
} from "../nativeFileJobs";
import { sectionDatabaseName, validateDeviceSectionId } from "./scopes";
import {
  defaultDeviceExportChoices,
  type DeviceSectionChoice,
} from "./selection";

function localize(choice: DeviceSectionChoice): DeviceSectionChoice {
  const text = language.portableBackup;
  return {
    ...choice,
    label:
      choice.sectionId === "local-storage"
        ? text.localStorage
        : choice.sectionId === "localforage"
          ? text.localData
          : choice.sectionId === "device-settings"
            ? text.settings
            : `${text.database}: ${sectionDatabaseName(choice.sectionId).slice("safe_plugin_".length)}`,
  };
}

let selectionOpen = false;
function showSelection(
  mode: "export" | "restore",
  choices: DeviceSectionChoice[],
  libraryIncluded: boolean,
  repairRequired = false,
  firstRun = false,
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
  const choices = await defaultDeviceExportChoices();
  choices.push({
    sectionId: "device-settings",
    label: language.portableBackup.settings,
    included: true,
    selected: false,
  });
  return showSelection("export", choices, true);
}

export function selectPortableBackupRestore(
  preview: NativePortableRestorePreview,
  options: { firstRun?: boolean } = {},
): Promise<NativePortableSelection | null> {
  const unique = new Set<string>();
  const choices = preview.deviceSections.map(
    (sectionId): DeviceSectionChoice => {
      validateDeviceSectionId(sectionId);
      if (unique.has(sectionId))
        throw new Error("Duplicate backup device section");
      unique.add(sectionId);
      return { sectionId, label: "", selected: true, included: true };
    },
  );
  return showSelection(
    "restore",
    choices,
    preview.libraryIncluded,
    preview.repairRequired,
    options.firstRun ?? false,
  );
}
