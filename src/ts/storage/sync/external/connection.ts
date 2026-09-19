import type {
    ExternalConnectionPurpose,
    ExternalJobSummary,
    ExternalHistoryItem,
    ExternalConflictSummary,
    ExternalOpenMode,
    ExternalCapturePolicy,
    ExternalProviderId,
    PrepareExternalConnectionRequest,
} from './types'
import { buildConnectionConfig, getExternalProviderDefinition } from './providerRegistry'

export const GITHUB_DEDICATED_REPOSITORY_ACKNOWLEDGEMENT = 'github-dedicated-private-repository'

/** Backup connections start with everything the device can contribute. */
export function defaultExternalCapturePolicy(
    purpose: ExternalConnectionPurpose,
): ExternalCapturePolicy | undefined {
    if (purpose !== 'backup') return undefined
    return { hypa: true, localPlugins: true, localSettings: true }
}

export function requiredConnectionAcknowledgements(
    providerId: ExternalProviderId,
): string[] {
    const acknowledgements: string[] = []
    if (providerId === 'github_releases')
        acknowledgements.push(GITHUB_DEDICATED_REPOSITORY_ACKNOWLEDGEMENT)
    return acknowledgements
}

export function buildPrepareConnectionRequest(options: {
    providerId: ExternalProviderId
    values: Record<string, string>
    platform: string
    mode: ExternalOpenMode
    purpose: ExternalConnectionPurpose
    capturePolicy?: ExternalCapturePolicy
    recoveryKey?: string
    acknowledgements: string[]
}): PrepareExternalConnectionRequest {
    const definition = getExternalProviderDefinition(options.providerId)
    if (options.purpose === 'sync' && !definition.supportsSync)
        throw new Error('This provider does not support synchronization.')
    if (options.purpose === 'sync' && options.capturePolicy)
        throw new Error('A synchronization connection does not carry a capture policy.')
    if (options.purpose === 'backup' && !options.capturePolicy)
        throw new Error('A backup connection needs a capture policy.')
    const missingAcknowledgement = requiredConnectionAcknowledgements(
        options.providerId,
    ).find(item => !options.acknowledgements.includes(item))
    if (missingAcknowledgement)
        throw new Error(`Required acknowledgement is missing: ${missingAcknowledgement}`)
    return {
        config: buildConnectionConfig(options.providerId, options.values, options.platform),
        mode: options.mode,
        purpose: options.purpose,
        ...(options.capturePolicy ? { capturePolicy: { ...options.capturePolicy } } : {}),
        ...(options.recoveryKey ? { recoveryKey: options.recoveryKey } : {}),
        acknowledgements: [...options.acknowledgements],
    }
}

export function externalJobIsActive(job: ExternalJobSummary): boolean {
    return job.state === 'queued' || job.state === 'running' || job.state === 'waiting'
}

export function externalJobProgress(job: ExternalJobSummary): number | null {
    const completed = Number(job.completedBytes)
    const total = Number(job.totalBytes)
    if (!Number.isFinite(completed) || !Number.isFinite(total) || total <= 0) return null
    return Math.max(0, Math.min(1, completed / total))
}

/**
 * Pages arrive grouped by repository object identifier, which carries no time
 * order, so the merged list is the only place that can put the newest first.
 */
export function mergeExternalHistoryItems(
    current: readonly ExternalHistoryItem[],
    next: readonly ExternalHistoryItem[],
): ExternalHistoryItem[] {
    const kindStrength: Record<ExternalHistoryItem['kind'], number> = {
        snapshot: 0,
        'recovery-candidate': 1,
        'backup-point': 2,
        conflict: 3,
    }
    const merged = new Map(current.map(item => [item.id, item]))
    for (const item of next) {
        const previous = merged.get(item.id)
        if (!previous) {
            merged.set(item.id, item)
            continue
        }
        merged.set(item.id, {
            ...previous,
            ...item,
            pinned: previous.pinned || item.pinned,
            kind: kindStrength[previous.kind] >= kindStrength[item.kind]
                ? previous.kind
                : item.kind,
        })
    }
    const retainedSnapshots = new Set([...merged.values()]
        .filter(item => item.kind === 'backup-point' || item.kind === 'conflict')
        .map(item => item.snapshotId ?? item.id))
    return [...merged.values()]
        .filter(item => item.kind !== 'recovery-candidate'
            || !retainedSnapshots.has(item.snapshotId ?? item.id))
        .sort((left, right) => {
        const difference = Number(right.createdAtMs) - Number(left.createdAtMs)
        return Number.isFinite(difference) ? difference : 0
        })
}

/** History entries a restore can actually read back. */
export function restorableExternalHistoryItems(
    items: readonly ExternalHistoryItem[],
): ExternalHistoryItem[] {
    return items.filter(item => item.complete && item.verified)
}

export type ExternalConflictAction = 'retry-sync' | 'keep-local' | 'use-remote'

export function externalConflictActions(
    conflict: ExternalConflictSummary,
): ExternalConflictAction[] {
    if (conflict.resolved) return []
    if (!conflict.remotePointConfirmed) return ['retry-sync']
    const actions: ExternalConflictAction[] = []
    if (conflict.localAvailable) actions.push('keep-local')
    if (conflict.remoteAvailable) actions.push('use-remote')
    return actions
}
