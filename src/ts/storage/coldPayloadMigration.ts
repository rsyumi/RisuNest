import { parseColdPayloadAuthorityState } from './coldPayloadAuthority'
import type { DurableAssetWriteSession, DurableAssetWriteSessionFactory } from './assetRepository'
import type { ColdPayloadStore } from './coldPayloadStore'
import { hashPayloadBytes, type ImmutablePayloadCas } from './payloadCas'
import {
    validateColdAlias,
    type ColdAlias,
    type ColdPayloadAuthorityState,
    type ColdPayloadMigrationInput,
    type DataRevision,
    type Versioned,
} from './persistentDataStore'

interface ColdPayloadMigrationLease {
    readonly revision: DataRevision
    readColdPayloadAuthority(): Promise<Versioned<ColdPayloadAuthorityState>>
    release(): Promise<void>
}

export interface ColdPayloadMigrationStore {
    readColdPayloadAuthority(): Promise<Versioned<ColdPayloadAuthorityState>>
    acquireRevision(revision: DataRevision): Promise<ColdPayloadMigrationLease>
    activateColdPayloadMigration(
        input: ColdPayloadMigrationInput,
    ): Promise<{ revision: DataRevision }>
}

export interface ColdPayloadMigrationResult {
    sourceRevision: DataRevision
    revision: DataRevision
    migrationId: string
    compatibilityHash: string
    aliases: number
}

async function inventoryHash(aliases: readonly ColdAlias[]): Promise<string> {
    const inventory = aliases.map(({ key, objectHash, size, metadata }) => ({
        key,
        objectHash,
        size,
        metadata,
    }))
    return hashPayloadBytes(new TextEncoder().encode(JSON.stringify(inventory)))
}

export async function migrateLegacyColdPayloads(input: {
    store: ColdPayloadMigrationStore
    legacy: ColdPayloadStore
    cas: ImmutablePayloadCas
    writeSessions?: DurableAssetWriteSessionFactory
    migrationId?: string
}): Promise<ColdPayloadMigrationResult> {
    const current = await input.store.readColdPayloadAuthority()
    if (parseColdPayloadAuthorityState(current.value).format !== 'legacy') {
        throw new Error('Cold payload migration requires legacy authority')
    }
    const sourceRevision = current.revision
    const lease = await input.store.acquireRevision(sourceRevision)
    let session: DurableAssetWriteSession | undefined
    let committed = false
    try {
        const pinned = await lease.readColdPayloadAuthority()
        if (
            pinned.revision !== sourceRevision
            || parseColdPayloadAuthorityState(pinned.value).format !== 'legacy'
        ) {
            throw new Error('Cold payload migration requires pinned legacy authority')
        }
        const keys = await input.legacy.list()
        session = await input.writeSessions?.begin()
        const aliases: ColdAlias[] = []
        let previous: string | undefined
        for (const key of [...keys].sort()) {
            if (key === previous) {
                throw new Error(`Duplicate legacy cold payload key during migration: ${key}`)
            }
            previous = key
            const data = await input.legacy.read(key)
            if (data === null) {
                throw new Error(`Legacy cold payload disappeared during migration: ${key}`)
            }
            const prepared = session
                ? await session.prepare(data)
                : await input.cas.prepare(data)
            if (prepared.byteSize !== data.byteLength) {
                throw new Error(`Cold payload CAS size mismatch during migration: ${key}`)
            }
            const alias: ColdAlias = {
                key,
                objectHash: prepared.contentHash,
                size: prepared.byteSize,
                metadata: {},
            }
            validateColdAlias(alias)
            aliases.push(alias)
        }
        const compatibilityHash = await inventoryHash(aliases)
        const migrationId = input.migrationId ?? globalThis.crypto.randomUUID()
        await session?.seal()
        const activated = await input.store.activateColdPayloadMigration({
            sourceRevision,
            migrationId,
            compatibilityHash,
            coldAliases: aliases,
        })
        committed = true
        await session?.release('committed')
        return {
            sourceRevision,
            revision: activated.revision,
            migrationId,
            compatibilityHash,
            aliases: aliases.length,
        }
    } catch (error) {
        if (session && !committed) {
            try {
                await session.release('aborted')
            } catch (releaseError) {
                throw new AggregateError(
                    [error, releaseError],
                    'Cold payload migration and durable CAS cleanup failed',
                )
            }
        }
        throw error
    } finally {
        await lease.release()
    }
}
