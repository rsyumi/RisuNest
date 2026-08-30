import type { Database } from './database.svelte'
import type { DurableAssetWriteSession, DurableAssetWriteSessionFactory } from './assetRepository'
import { inferBlobMime, type BlobMetadata, type BlobStore } from './blobStore'
import { encodeOwnerManifest, ownerManifestIdentity, type AssetTuple } from './ownerManifestCodec'
import { hashPayloadBytes, type ImmutablePayloadCas } from './payloadCas'
import { canonicalJson } from './saveCoordinator'
import {
    validateAssetAlias,
    type AssetAlias,
    type AssetOwnerHead,
    type AssetOwnerLocator,
    type PersistentDataStore,
} from './persistentDataStore'

export interface AssetRepositoryMigrationResult {
    sourceRevision: number
    revision: number
    migrationId: string
    compatibilityHash: string
    aliases: number
    ownerHeads: number
}

function aliasIdentity(kind: AssetAlias['kind'], key: string): string {
    return `${kind}\0${key}`
}

function aliasFromMetadata(metadata: BlobMetadata, objectHash: string): AssetAlias {
    const alias = { ...metadata, objectHash } as AssetAlias
    validateAssetAlias(alias)
    return alias
}

function requireAssetTuple(value: unknown): AssetTuple {
    if (
        !Array.isArray(value)
        || value.length !== 3
        || value.some((entry) => typeof entry !== 'string')
    ) {
        throw new TypeError('Asset owner entries must be string triples')
    }
    return [value[0], value[1], value[2]]
}

async function prepareOwnerHead(
    cas: ImmutablePayloadCas,
    session: DurableAssetWriteSession | undefined,
    aliases: Map<string, AssetAlias>,
    owner: AssetOwnerLocator,
    parent: object,
    property: string,
): Promise<AssetOwnerHead> {
    if (!Object.prototype.hasOwnProperty.call(parent, property)) {
        return { owner, present: false, manifestHash: null, entryCount: 0 }
    }
    const values = (parent as Record<string, unknown>)[property]
    if (!Array.isArray(values)) {
        throw new TypeError(`Asset owner ${property} property must be an array when present`)
    }
    const entries = values.map((value) => {
        const tuple = requireAssetTuple(value)
        const identity = aliasIdentity('asset', tuple[1])
        let alias = aliases.get(identity)
        if (!alias) {
            alias = {
                kind: 'asset',
                key: tuple[1],
                objectHash: null,
                size: 0,
                mime: inferBlobMime(undefined, tuple[2]),
                name: tuple[0],
                ext: tuple[2],
            }
            validateAssetAlias(alias)
            aliases.set(identity, alias)
        }
        return {
            tuple,
            payloadHash: alias.objectHash === null
                ? null
                : Uint8Array.from(Buffer.from(alias.objectHash, 'hex')),
        }
    })
    const bytes = encodeOwnerManifest(entries)
    const prepared = session
        ? await session.prepare(bytes, 'owner-manifest')
        : await cas.prepare(bytes)
    const manifestHash = await ownerManifestIdentity(bytes)
    if (prepared.contentHash !== manifestHash || prepared.byteSize !== bytes.byteLength) {
        throw new Error('Owner manifest CAS identity mismatch')
    }
    return { owner, present: true, manifestHash, entryCount: entries.length }
}

async function prepareOwnerHeads(
    database: Database,
    cas: ImmutablePayloadCas,
    session: DurableAssetWriteSession | undefined,
    aliases: Map<string, AssetAlias>,
): Promise<AssetOwnerHead[]> {
    const heads: AssetOwnerHead[] = []
    for (const [index, module] of (database.modules ?? []).entries()) {
        heads.push(await prepareOwnerHead(
            cas,
            session,
            aliases,
            { kind: 'root-module-assets', index },
            module,
            'assets',
        ))
    }
    for (const [index, persona] of (database.personas ?? []).entries()) {
        if (!persona.embeddedModule) continue
        heads.push(await prepareOwnerHead(
            cas,
            session,
            aliases,
            { kind: 'persona-embedded-module-assets', index },
            persona.embeddedModule,
            'assets',
        ))
    }
    for (const character of database.characters) {
        heads.push(await prepareOwnerHead(
            cas,
            session,
            aliases,
            { kind: 'character-additional-assets', characterId: character.chaId },
            character,
            'additionalAssets',
        ))
    }
    return heads
}

export async function migrateLegacyAssetRepository(input: {
    store: PersistentDataStore
    legacy: BlobStore
    cas: ImmutablePayloadCas
    writeSessions?: DurableAssetWriteSessionFactory
    migrationId?: string
}): Promise<AssetRepositoryMigrationResult> {
    const authority = await input.store.readAssetRepositoryAuthority()
    if (authority.value.format !== 'legacy') {
        throw new Error('Asset repository migration requires legacy authority')
    }
    const sourceRevision = authority.revision
    const lease = await input.store.acquireRevision(sourceRevision)
    let session: DurableAssetWriteSession | undefined
    let committed = false
    try {
        const database = await input.store.materializeDatabase(sourceRevision)
        session = await input.writeSessions?.begin()
        const aliases = new Map<string, AssetAlias>()
        for (const metadata of await input.legacy.list()) {
            const data = await input.legacy.read(metadata.key)
            if (data === null) {
                throw new Error(`Legacy asset disappeared during migration: ${metadata.key}`)
            }
            const prepared = session
                ? await session.prepare(data)
                : await input.cas.prepare(data)
            if (prepared.byteSize !== data.byteLength) {
                throw new Error(`CAS size mismatch during migration: ${metadata.key}`)
            }
            const alias = aliasFromMetadata(metadata, prepared.contentHash)
            const identity = aliasIdentity(alias.kind, alias.key)
            if (aliases.has(identity)) {
                throw new Error(`Duplicate legacy asset identity during migration: ${metadata.key}`)
            }
            aliases.set(identity, alias)
        }
        const ownerHeads = await prepareOwnerHeads(database, input.cas, session, aliases)
        for (const alias of aliases.values()) {
            if (alias.objectHash === null) continue
            if (await input.cas.statObject(alias.objectHash) !== alias.size) {
                throw new Error(`CAS verification failed during migration: ${alias.key}`)
            }
        }
        const compatibilityHash = await hashPayloadBytes(
            new TextEncoder().encode(canonicalJson(database)),
        )
        const migrationId = input.migrationId ?? globalThis.crypto.randomUUID()
        await session?.seal()
        const activated = await input.store.activateAssetRepositoryMigration({
            sourceRevision,
            migrationId,
            compatibilityHash,
            database,
            assetAliases: [...aliases.values()],
            assetOwnerHeads: ownerHeads,
        })
        committed = true
        await session?.release('committed')
        return {
            sourceRevision,
            revision: activated.revision,
            migrationId,
            compatibilityHash,
            aliases: aliases.size,
            ownerHeads: ownerHeads.length,
        }
    } catch (error) {
        if (session && !committed) {
            try {
                await session.release('aborted')
            } catch (releaseError) {
                throw new AggregateError(
                    [error, releaseError],
                    'Asset migration and durable CAS cleanup failed',
                )
            }
        }
        throw error
    } finally {
        await lease.release()
    }
}
