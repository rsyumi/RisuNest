import { normalizeBlobExtension, type InlayBlobMetadata, type InlayEncodeOptions } from "../../storage/blobStore";
import type { NewInlayImageEncoder } from "../../storage/assetRepository";

export interface InlayOptimizationProgress {
    scanned: number
    total: number
    converted: number
    skipped: number
    failed: number
    /** Original and new size of the converted entries only. */
    beforeBytes: number
    afterBytes: number
}

export interface InlayOptimizationDeps {
    read(key: string): Promise<Uint8Array | null>
    encoder: NewInlayImageEncoder
    write(
        key: string,
        data: Uint8Array,
        metadata: Omit<InlayBlobMetadata, 'key' | 'size'>,
    ): Promise<unknown>
}

export interface InlayOptimizationRun {
    /** Quality and maximum resolution come from the user settings; the format is always WebP. */
    options: InlayEncodeOptions
    onProgress?(progress: InlayOptimizationProgress): void
    isCancelled?(): boolean
}

/** A new image must reach this share of the original to be worth the quality loss. */
export const inlayOptimizationGainRatio = 0.95

/**
 * Images that are not WebP yet, plus WebP images above the resolution limit.
 * Re-encoding an image that is already within the limit only loses quality, so
 * running the job twice in a row leaves the second run with nothing to do.
 */
export function selectInlayOptimizationTargets(
    metadata: readonly InlayBlobMetadata[],
    options: Pick<InlayEncodeOptions, 'maxDimension'>,
): InlayBlobMetadata[] {
    return metadata.filter((item) => {
        if (item.inlayType !== 'image') return false
        if (normalizeBlobExtension(item.ext) !== 'webp') return true
        if (options.maxDimension <= 0) return false
        return Math.max(item.width ?? 0, item.height ?? 0) > options.maxDimension
    })
}

export function emptyInlayOptimizationProgress(total = 0): InlayOptimizationProgress {
    return { scanned: 0, total, converted: 0, skipped: 0, failed: 0, beforeBytes: 0, afterBytes: 0 }
}

/**
 * Converts one entry at a time and keeps the original whenever the new image is
 * not clearly smaller, cannot be read, or cannot be re-encoded. Only a failed
 * write counts as a failure; everything else is a skip that leaves the stored
 * image untouched.
 */
export async function runInlayOptimization(
    targets: readonly InlayBlobMetadata[],
    deps: InlayOptimizationDeps,
    run: InlayOptimizationRun,
): Promise<InlayOptimizationProgress> {
    const progress = emptyInlayOptimizationProgress(targets.length)
    const options: InlayEncodeOptions = { ...run.options, format: 'webp', skipReencode: false }
    for (const target of targets) {
        if (run.isCancelled?.()) break
        let prepared: { source: Uint8Array, encoded: Awaited<ReturnType<NewInlayImageEncoder['encodeNewInlayImage']>> } | null = null
        try {
            const source = await deps.read(target.key)
            if (source) {
                const encoded = await deps.encoder.encodeNewInlayImage(target.key, source, { name: target.name, options })
                if (encoded.data.byteLength <= source.byteLength * inlayOptimizationGainRatio) prepared = { source, encoded }
            }
        } catch (error) {
            void error
            prepared = null
        }
        if (!prepared) progress.skipped += 1
        else {
            try {
                await deps.write(target.key, prepared.encoded.data, prepared.encoded.metadata)
                progress.converted += 1
                progress.beforeBytes += prepared.source.byteLength
                progress.afterBytes += prepared.encoded.data.byteLength
            } catch (error) {
                void error
                progress.failed += 1
            }
        }
        progress.scanned += 1
        run.onProgress?.({ ...progress })
    }
    return { ...progress }
}

export type InlayOptimizationWarning = 'format' | 'sync' | 'remote'

/** Facts the confirmation has to state before an irreversible run. */
export function inlayOptimizationWarnings(input: {
    storedFormat: InlayEncodeOptions['format']
    syncConfigured: boolean
    residencyPolicy?: 'full' | 'remote'
}): InlayOptimizationWarning[] {
    const warnings: InlayOptimizationWarning[] = []
    if (input.storedFormat !== 'webp') warnings.push('format')
    if (input.syncConfigured) warnings.push('sync')
    if (input.syncConfigured && input.residencyPolicy === 'remote') warnings.push('remote')
    return warnings
}
