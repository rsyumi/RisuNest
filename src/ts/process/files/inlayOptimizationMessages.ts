import { language } from "src/lang";
import { formatRisuNestStorageBytes } from "../../storage/risuNestStorageDashboard";
import type { InlayBlobMetadata, InlayEncodeFormat } from "../../storage/blobStore";
import {
    inlayOptimizationWarnings,
    type InlayOptimizationProgress,
    type InlayOptimizationWarning,
} from "./inlayOptimizationJob";

export interface InlayOptimizationEnvironment {
    syncConfigured: boolean
    residencyPolicy?: 'full' | 'remote'
}

function warningText(warning: InlayOptimizationWarning): string {
    const strings = language.risuNest.inlay
    if (warning === 'format') return strings.optimizeFormatNotice
    if (warning === 'sync') return strings.optimizeSyncNotice
    return strings.optimizeRemoteNotice
}

/** The confirmation an irreversible run needs: what it touches and what follows. */
export function inlayOptimizationConfirmMessage(input: {
    targets: readonly InlayBlobMetadata[]
    storedFormat: InlayEncodeFormat
} & InlayOptimizationEnvironment): string {
    const bytes = input.targets.reduce((carry, target) => carry + target.size, 0)
    return [
        language.risuNest.inlay.optimizeConfirm
            .replace('{count}', input.targets.length.toLocaleString())
            .replace('{size}', formatRisuNestStorageBytes(bytes)),
        ...inlayOptimizationWarnings({
            storedFormat: input.storedFormat,
            syncConfigured: input.syncConfigured,
            residencyPolicy: input.residencyPolicy,
        }).map(warningText),
    ].join('\n\n')
}

export function inlayOptimizationProgressMessage(progress: InlayOptimizationProgress): string {
    return language.risuNest.inlay.optimizeProgress
        .replace('{done}', progress.scanned.toLocaleString())
        .replace('{total}', progress.total.toLocaleString())
}

export function inlayOptimizationResultMessage(progress: InlayOptimizationProgress): string {
    return language.risuNest.inlay.optimizeDone
        .replace('{converted}', progress.converted.toLocaleString())
        .replace('{saved}', formatRisuNestStorageBytes(Math.max(0, progress.beforeBytes - progress.afterBytes)))
        .replace('{skipped}', progress.skipped.toLocaleString())
        .replace('{failed}', progress.failed.toLocaleString())
}
