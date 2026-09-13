import { invoke } from "@tauri-apps/api/core";

export type AssetResidencyPolicy = "full" | "remote";
export interface AssetResidencyStatus {
  policy: AssetResidencyPolicy;
  localBytes: number;
  remoteBytes: number;
  remoteObjects: number;
  unavailableObjects: number;
  evictedBytes: number;
}
async function command(
  name: string,
  args?: Record<string, unknown>,
): Promise<AssetResidencyStatus> {
  const result = await invoke<AssetResidencyStatus>(name, args);
  if (
    !result ||
    !["full", "remote"].includes(result.policy) ||
    [
      result.localBytes,
      result.remoteBytes,
      result.remoteObjects,
      result.unavailableObjects,
      result.evictedBytes,
    ].some((value) => !Number.isSafeInteger(value) || value < 0)
  ) {
    throw new Error("invalid-asset-residency-status");
  }
  return result;
}
export const getAssetResidencyStatus = () =>
  command("server_sync_asset_status");
export const setAssetResidencyPolicy = (policy: AssetResidencyPolicy) =>
  command("server_sync_asset_policy", { policy });
export const evictLocalAssets = () => command("server_sync_asset_evict");
export const cancelAssetResidencyOperation = () =>
  invoke<void>("server_sync_cancel");
