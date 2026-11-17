import type {
    ExternalConnectionPurpose,
    ExternalJobSummary,
    ExternalHistoryItem,
    ExternalOpenMode,
    ExternalProviderId,
    PrepareExternalConnectionRequest,
} from './types'
import { buildConnectionConfig, getExternalProviderDefinition } from './providerRegistry'

export const GITHUB_DEDICATED_REPOSITORY_ACKNOWLEDGEMENT = 'github-dedicated-private-repository'

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
    recoveryKey?: string
    acknowledgements: string[]
}): PrepareExternalConnectionRequest {
    const definition = getExternalProviderDefinition(options.providerId)
    if (options.purpose === 'sync' && !definition.supportsSync)
        throw new Error('This provider does not support synchronization.')
    const missingAcknowledgement = requiredConnectionAcknowledgements(
        options.providerId,
    ).find(item => !options.acknowledgements.includes(item))
    if (missingAcknowledgement)
        throw new Error(`Required acknowledgement is missing: ${missingAcknowledgement}`)
    return {
        config: buildConnectionConfig(options.providerId, options.values, options.platform),
        mode: options.mode,
        purpose: options.purpose,
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
        .map(item => item.snapshotId))
    return [...merged.values()]
        .filter(item => item.kind !== 'recovery-candidate'
            || !retainedSnapshots.has(item.snapshotId))
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


/** Native failure kind of a rejected command (`kind`) or of a job error DTO (`code`). */
export function externalErrorKind(value: unknown): string | undefined {
    if (typeof value !== 'object' || value === null) return undefined
    const carrier = value as { kind?: unknown; code?: unknown }
    const kind = typeof carrier.kind === 'string' ? carrier.kind : carrier.code
    return typeof kind === 'string' ? kind : undefined
}

/** The code an asset download reports when the provider asks to sign in again. */
export const EXTERNAL_ASSET_REAUTH_CODE = 'external-reauth-required'
/** The code an asset download reports when the storage that holds a body has no recovery key on this device. */
export const EXTERNAL_ASSET_KEY_CODE = 'external-key-locked'

/** Whether signing in again, or entering a new key for a key-based provider, clears this failure. */
export function externalNeedsRenewal(value: unknown): boolean {
    if (typeof value !== 'object' || value === null) return false
    if ((value as { action?: unknown }).action === 'reauthenticate') return true
    return ['reauthRequired', 'unauthorized', EXTERNAL_ASSET_REAUTH_CODE].includes(externalErrorKind(value) ?? '')
}

/** The connection that has to sign in again, or take a new key, to clear a failure `connectionId` reported. An
 * asset download reads every connection that holds a body, so its failure names the one that refused. */
export function externalRenewalConnection(value: unknown, connectionId: string): string | undefined {
    return externalNeedsRenewal(value) ? namedConnection(value, connectionId) : undefined
}

/** Whether entering the recovery key clears this failure. */
export function externalNeedsUnlock(value: unknown): boolean {
    if (typeof value !== 'object' || value === null) return false
    return ['repositoryKeyUnavailable', 'recoveryKeyMismatch', EXTERNAL_ASSET_KEY_CODE].includes(externalErrorKind(value) ?? '')
}

/** The connection whose sign-in or recovery key clears a failure `connectionId` reported, named the same way. */
export function externalRemedyConnection(value: unknown, connectionId: string): string | undefined {
    return externalNeedsRenewal(value) || externalNeedsUnlock(value) ? namedConnection(value, connectionId) : undefined
}

function namedConnection(value: unknown, connectionId: string): string {
    const named = (value as { connectionId?: unknown }).connectionId
    return typeof named === 'string' && named ? named : connectionId
}

/** The connection whose sign-in or key clears a job's error: the one the error names, or the job's own. */
export function externalJobFixedOn(job: ExternalJobSummary): string {
    return job.error?.connectionId || job.connectionId
}

export function externalJobIsPaused(job: ExternalJobSummary): boolean {
    return job.state === 'waiting'
        && job.phase === 'paused'
}
