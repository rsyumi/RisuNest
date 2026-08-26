import type { ColdPayloadStore } from './coldPayloadStore'
import type { ImmutablePayloadCas } from './payloadCas'
import {
    RevisionConflictError,
    validateColdAlias,
    type ColdAlias,
    type DataRevision,
    type PersistentRoot,
    type Versioned,
} from './persistentDataStore'

export interface ColdAliasCatalog {
    readRoot(): Promise<Versioned<PersistentRoot>>
    readColdAlias(key: string): Promise<Versioned<ColdAlias> | null>
    listColdAliases(): Promise<Versioned<ColdAlias[]>>
    commitColdAlias(
        alias: ColdAlias,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }>
    deleteColdAlias(
        key: string,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }>
}

function validateKey(key: string): void {
    validateColdAlias({ key, objectHash: null, size: 0, metadata: {} })
}

function validateVersionedAlias(
    versioned: Versioned<ColdAlias> | null,
    key: string,
): Versioned<ColdAlias> | null {
    if (!versioned) return null
    validateColdAlias(versioned.value)
    if (versioned.value.key !== key) {
        throw new TypeError('Cold alias does not match its requested key')
    }
    return versioned
}

export function createCompleteColdPayloadStore(input: {
    catalog: ColdAliasCatalog
    cas: ImmutablePayloadCas
    legacy: ColdPayloadStore
}): ColdPayloadStore {
    return {
        async read(key) {
            validateKey(key)
            const versioned = validateVersionedAlias(await input.catalog.readColdAlias(key), key)
            if (!versioned) return null
            const alias = versioned.value
            if (alias.objectHash === null) {
                const legacy = await input.legacy.read(key)
                if (legacy === null) return null
                if (legacy.byteLength !== alias.size) {
                    throw new Error(`Cold payload legacy size mismatch for ${key}`)
                }
                return legacy
            }
            const size = await input.cas.statObject(alias.objectHash)
            if (size === null) {
                throw new Error(`Cold payload immutable object is missing for ${key}`)
            }
            if (size !== alias.size) {
                throw new Error(`Cold payload immutable object size mismatch for ${key}`)
            }
            const data = await input.cas.readObject(alias.objectHash)
            if (data === null) {
                throw new Error(`Cold payload immutable object is missing for ${key}`)
            }
            if (data.byteLength !== alias.size) {
                throw new Error(`Cold payload immutable object size mismatch for ${key}`)
            }
            return data
        },
        async write(key, data) {
            validateKey(key)
            const ownedData = data.slice()
            const prepared = await input.cas.prepare(ownedData)
            if (prepared.byteSize !== ownedData.byteLength) {
                throw new Error(`Prepared cold payload size mismatch for ${key}`)
            }
            for (;;) {
                const existing = validateVersionedAlias(
                    await input.catalog.readColdAlias(key),
                    key,
                )
                const revision = existing?.revision ?? (await input.catalog.readRoot()).revision
                const alias: ColdAlias = {
                    key,
                    objectHash: prepared.contentHash,
                    size: prepared.byteSize,
                    metadata: structuredClone(existing?.value.metadata ?? {}),
                }
                validateColdAlias(alias)
                try {
                    await input.catalog.commitColdAlias(alias, revision)
                    return
                } catch (error) {
                    if (!(error instanceof RevisionConflictError)) throw error
                }
            }
        },
        async list() {
            const aliases = await input.catalog.listColdAliases()
            const keys = aliases.value.map((alias) => {
                validateColdAlias(alias)
                return alias.key
            })
            return keys.sort()
        },
        async remove(key) {
            validateKey(key)
            for (;;) {
                const existing = validateVersionedAlias(
                    await input.catalog.readColdAlias(key),
                    key,
                )
                if (!existing) return
                try {
                    await input.catalog.deleteColdAlias(key, existing.revision)
                    return
                } catch (error) {
                    if (!(error instanceof RevisionConflictError)) throw error
                }
            }
        },
    }
}
