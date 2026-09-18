import {
  databaseSectionId,
  sectionDatabaseName,
  validateDeviceSectionId,
  type DeviceSectionId,
  type DeviceSectionInfo,
} from "./scopes";

export interface DeviceSectionChoice<TSectionId extends string = string> {
  sectionId: TSectionId;
  label: string;
  selected: boolean;
  included: boolean;
  recordCount?: number;
}

export const nativePortableDeviceSections = [
  "hypa",
  "local-plugins",
  "local-settings",
] as const;

export type NativePortableDeviceSection =
  (typeof nativePortableDeviceSections)[number];

export function validateNativePortableDeviceSection(
  value: string,
): asserts value is NativePortableDeviceSection {
  if (!(nativePortableDeviceSections as readonly string[]).includes(value))
    throw new Error(`Invalid native portable device section: ${value}`);
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
): Promise<DeviceSectionChoice<DeviceSectionId>[]> {
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
): DeviceSectionChoice<DeviceSectionId>[] {
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
  choices: readonly DeviceSectionChoice<DeviceSectionId>[],
): DeviceSectionId[] {
  const selected = choices
    .filter((choice) => choice.included && choice.selected)
    .map((choice) => choice.sectionId);
  for (const section of selected) validateDeviceSectionId(section);
  if (new Set(selected).size !== selected.length)
    throw new Error("Duplicate device selection");
  return selected;
}

export function defaultNativePortableExportChoices(): DeviceSectionChoice<NativePortableDeviceSection>[] {
  return nativePortableDeviceSections.map((sectionId) => ({
    sectionId,
    label: "",
    included: true,
    selected: sectionId !== "local-settings",
  }));
}

export function defaultNativePortableRestoreChoices(
  sections: readonly string[],
): DeviceSectionChoice<NativePortableDeviceSection>[] {
  const unique = new Set<string>();
  return sections.map((sectionId) => {
    validateNativePortableDeviceSection(sectionId);
    if (unique.has(sectionId))
      throw new Error("Duplicate native portable device section");
    unique.add(sectionId);
    return {
      sectionId,
      label: "",
      included: true,
      selected: true,
    };
  });
}
