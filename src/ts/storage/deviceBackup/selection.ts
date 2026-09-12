import {
  databaseSectionId,
  sectionDatabaseName,
  validateDeviceSectionId,
  type DeviceSectionId,
  type DeviceSectionInfo,
} from "./scopes";

export interface DeviceSectionChoice {
  sectionId: DeviceSectionId;
  label: string;
  selected: boolean;
  included: boolean;
  recordCount?: number;
}

export function deviceSectionLabel(sectionId: DeviceSectionId): string {
  if (sectionId === "local-storage") return "Plugin local storage";
  if (sectionId === "localforage") return "Plugin local data";
  if (sectionId === "device-settings") return "Device settings";
  return `Plugin IndexedDB: ${sectionDatabaseName(sectionId).slice("safe_plugin_".length)}`;
}

/** Read-only selection preview. Consistent capture starts only after native maintenance. */
export async function defaultDeviceExportChoices(
  factory: IDBFactory = indexedDB,
): Promise<DeviceSectionChoice[]> {
  if (typeof factory.databases !== "function")
    throw new Error("Plugin database enumeration is unavailable");
  const names = (await factory.databases())
    .flatMap(({ name }) => (name?.startsWith("safe_plugin_") ? [name] : []))
    .sort();
  const sections: DeviceSectionId[] = [
    "local-storage",
    "localforage",
    ...names.map(databaseSectionId),
  ];
  return sections.map((sectionId) => ({
    sectionId,
    label: deviceSectionLabel(sectionId),
    selected: true,
    included: true,
  }));
}

export function defaultDeviceRestoreChoices(
  sections: readonly DeviceSectionInfo[],
): DeviceSectionChoice[] {
  const unique = new Set<string>();
  return sections.map((section) => {
    validateDeviceSectionId(section.sectionId);
    if (unique.has(section.sectionId))
      throw new Error("Duplicate device section");
    unique.add(section.sectionId);
    return {
      sectionId: section.sectionId,
      label: deviceSectionLabel(section.sectionId),
      included: true,
      selected: true,
      recordCount: section.recordCount,
    };
  });
}

export function selectedDeviceSections(
  choices: readonly DeviceSectionChoice[],
): DeviceSectionId[] {
  const selected = choices
    .filter((choice) => choice.included && choice.selected)
    .map((choice) => choice.sectionId);
  for (const section of selected) validateDeviceSectionId(section);
  if (new Set(selected).size !== selected.length)
    throw new Error("Duplicate device selection");
  return selected;
}
