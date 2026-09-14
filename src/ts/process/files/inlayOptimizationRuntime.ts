import { isTauri } from "../../platform";
import { resolveBlobStore } from "../../storage/platformBlobStore";
import { getServerSyncController } from "../../storage/sync/serverSyncProduction";
import { getAssetResidencyStatus } from "../../storage/sync/serverAssetResidency";
import type { InlayOptimizationEnvironment } from "./inlayOptimizationMessages";
import { resolveInlayImageEncoder } from "./inlayImageEncoding";
import type { InlayOptimizationDeps } from "./inlayOptimizationJob";

/** Binds the optimization job to the live blob store and the platform encoder. */
export function createStoredInlayOptimizationDeps(): InlayOptimizationDeps {
    return {
        async read(key) { return (await resolveBlobStore()).read(key) },
        encoder: resolveInlayImageEncoder(),
        async write(key, data, metadata) { return (await resolveBlobStore()).put(key, data, metadata) },
    }
}

/** Whether converted images have to travel to a server, and whether they have to come back first. */
export async function readInlayOptimizationEnvironment(): Promise<InlayOptimizationEnvironment> {
    if (!isTauri) return { syncConfigured: false }
    try {
        const syncConfigured = getServerSyncController().snapshot().status?.configured === true
        if (!syncConfigured) return { syncConfigured }
        return { syncConfigured, residencyPolicy: (await getAssetResidencyStatus()).policy }
    } catch (error) {
        void error
        return { syncConfigured: false }
    }
}
