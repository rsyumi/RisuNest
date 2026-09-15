import { isTauri } from "../../platform";
import { resolveBlobStore } from "../../storage/platformBlobStore";
import { getServerSyncController } from "../../storage/sync/serverSyncProduction";
import { getAssetResidencyStatus } from "../../storage/sync/serverAssetResidency";
import type { InlayOptimizationEnvironment } from "./inlayOptimizationMessages";
import { encodeInlayImageBytes } from "./inlayImageEncoding";
import {
    createNativeNewInlayImageEncoder,
} from "../../storage/nativeAssetRepository";
import {
    defaultInlayEncodeOptions,
    type InlayBlobMetadata,
} from "../../storage/blobStore";
import type { NewInlayImageEncoder } from "../../storage/assetRepository";
import type { InlayOptimizationDeps } from "./inlayOptimizationJob";

function webInlayImageEncoder(): NewInlayImageEncoder {
    return {
        async encodeNewInlayImage(key, data, input) {
            const options = input.options ?? defaultInlayEncodeOptions
            const encoded = await encodeInlayImageBytes(data, options)
            const metadata: Omit<InlayBlobMetadata, 'key' | 'size'> = {
                kind: 'inlay',
                inlayType: 'image',
                mime: encoded.mime,
                name: input.name,
                ext: encoded.ext,
                width: encoded.width,
                height: encoded.height,
            }
            return { data: encoded.data, metadata }
        },
    }
}

/** The encoder that re-encodes without writing, native where one exists. */
export function resolveInlayImageEncoder(): NewInlayImageEncoder {
    return isTauri ? createNativeNewInlayImageEncoder() : webInlayImageEncoder()
}

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
