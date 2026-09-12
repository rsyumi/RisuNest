import localforage from "localforage";

/** Public plugin storage boundary, shared with the isolated maintenance entry. */
export const pluginDeviceStorage = localforage.createInstance({
  name: "plugin",
  storeName: "plugin",
});

export const pluginDevicePrefix = "safe_plugin_";
