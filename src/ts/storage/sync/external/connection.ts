import type {
    ExternalConnectionPurpose,
    ExternalJobSummary,
    ExternalHistoryItem,
    ExternalConflictSummary,
    ExternalOpenMode,
    ExternalProviderId,
    ExternalPublicationStrategy,
    ExternalStorageScope,
    PrepareExternalConnectionRequest,
} from './types'
import { buildConnectionConfig, getExternalProviderDefinition } from './providerRegistry'

export const SEQUENTIAL_ACKNOWLEDGEMENT = 'sequential-single-device'
export const GITHUB_DEDICATED_REPOSITORY_ACKNOWLEDGEMENT = 'github-dedicated-private-repository'

export function defaultExternalStorageScope(purpose: ExternalConnectionPurpose): ExternalStorageScope {
    return {
        library: true,
        referencedAssets: true,
        deviceSettings: purpose === 'backup',
        devicePlugins: false,
    }
}

export function requiredConnectionAcknowledgements(
    providerId: ExternalProviderId,
    strategy: ExternalPublicationStrategy,
): string[] {
    const acknowledgements: string[] = []
    if (strategy === 'sequential') acknowledgements.push(SEQUENTIAL_ACKNOWLEDGEMENT)
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
    strategy: ExternalPublicationStrategy
    scope: ExternalStorageScope
    acknowledgements: string[]
}): PrepareExternalConnectionRequest {
    const definition = getExternalProviderDefinition(options.providerId)
    if (!definition.strategies.includes(options.strategy))
        throw new Error(`${options.providerId} does not support ${options.strategy}.`)
    if (options.purpose === 'sync' && options.strategy === 'backup-only')
        throw new Error('A synchronization connection needs a synchronization strategy.')
    if (options.purpose === 'backup' && options.strategy !== 'backup-only')
        throw new Error('A backup destination must use the backup-only strategy.')
    if (options.purpose === 'sync' && (options.scope.deviceSettings || options.scope.devicePlugins))
        throw new Error('A synchronization repository cannot include device-only sections.')
    const missingAcknowledgement = requiredConnectionAcknowledgements(
        options.providerId,
        options.strategy,
    ).find(item => !options.acknowledgements.includes(item))
    if (missingAcknowledgement)
        throw new Error(`Required acknowledgement is missing: ${missingAcknowledgement}`)
    return {
        config: buildConnectionConfig(options.providerId, options.values, options.platform),
        mode: options.mode,
        purpose: options.purpose,
        publicationStrategy: options.strategy,
        scope: options.scope,
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
    return [...merged.values()].sort((left, right) => {
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
    if (conflict.preservation === 'local-only') return ['retry-sync']
    if (conflict.remoteRevision === null) return []
    return ['keep-local', 'use-remote']
}
