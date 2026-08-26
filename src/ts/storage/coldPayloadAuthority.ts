import type { ColdPayloadAuthorityState } from './persistentDataStore'
import type { ColdPayloadStore } from './coldPayloadStore'

export type { ColdPayloadAuthorityState } from './persistentDataStore'

export interface ColdPayloadAuthorityStores {
    legacy: ColdPayloadStore
    v2?: ColdPayloadStore
    v2Capability?: boolean
}

function stateRecord(value: unknown): Record<string, unknown> {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
        throw new TypeError('Cold payload authority state must be an object')
    }
    return value as Record<string, unknown>
}

function requireExactKeys(record: Record<string, unknown>, expected: string[]): void {
    const actual = Object.keys(record).sort()
    const sortedExpected = [...expected].sort()
    if (
        actual.length !== sortedExpected.length
        || actual.some((key, index) => key !== sortedExpected[index])
    ) {
        throw new TypeError('Cold payload authority state fields are invalid')
    }
}

function migrationId(value: unknown): string {
    if (
        typeof value !== 'string'
        || value.length === 0
        || value.length > 64
        || !/^[A-Za-z0-9_-]+$/.test(value)
    ) {
        throw new TypeError('Cold payload migrationId is invalid')
    }
    return value
}

export function parseColdPayloadAuthorityState(
    value: unknown,
): ColdPayloadAuthorityState {
    const record = stateRecord(value)
    if (record.format === 'legacy') {
        requireExactKeys(record, ['format'])
        return { format: 'legacy' }
    }
    if (record.format === 'preparing') {
        requireExactKeys(record, ['format', 'migrationId', 'sourceRevision'])
        const sourceRevision = record.sourceRevision
        if (
            typeof sourceRevision !== 'number'
            || !Number.isSafeInteger(sourceRevision)
            || sourceRevision < 0
        ) {
            throw new TypeError('Cold payload sourceRevision is invalid')
        }
        return {
            format: 'preparing',
            migrationId: migrationId(record.migrationId),
            sourceRevision,
        }
    }
    if (record.format === 'v2') {
        requireExactKeys(record, ['format', 'migrationId', 'compatibilityHash'])
        if (
            typeof record.compatibilityHash !== 'string'
            || !/^[0-9a-f]{64}$/.test(record.compatibilityHash)
        ) {
            throw new TypeError('Cold payload compatibilityHash is invalid')
        }
        return {
            format: 'v2',
            migrationId: migrationId(record.migrationId),
            compatibilityHash: record.compatibilityHash,
        }
    }
    throw new TypeError('Cold payload authority format is invalid')
}

export function selectColdPayloadAuthority(
    state: unknown,
    stores: ColdPayloadAuthorityStores,
): ColdPayloadStore {
    const parsed = parseColdPayloadAuthorityState(state)
    if (parsed.format === 'legacy') return stores.legacy
    if (parsed.format === 'preparing') {
        throw new Error('Cold payload preparing generation cannot be selected as authority')
    }
    if (stores.v2Capability !== true || !stores.v2 || stores.v2 === stores.legacy) {
        throw new Error('Cold payload v2 capability is unavailable, refusing legacy fallback')
    }
    return stores.v2
}
