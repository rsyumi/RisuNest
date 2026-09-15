import { assetName } from "./common.mjs";
import { requiredDownloads } from "./contracts.mjs";

export function releaseDownloads(product, version) {
  return requiredDownloads(product).map((row) => ({
    ...row,
    fileName: assetName({ ...row, version }),
  }));
}
