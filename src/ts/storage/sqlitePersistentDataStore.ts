import { invoke } from '@tauri-apps/api/core'

import type { Chat, Database, botPreset } from './database.svelte'
import {
    nativePersistentRevisionLease,
    type NativePersistentRevisionLease,
} from './nativePersistentExport'
import {
    RevisionConflictError,
    SnapshotReleasedError,
    validateConversationWindowQuery,
    type AssetAlias,
    type AssetOwnerHead,
    type AssetOwnerLocator,
    type CharacterDetail,
    type CharacterPage,
    type CharacterQuery,
    type ConversationPage,
    type ConversationQuery,
    type ConversationWindow,
    type ConversationWindowQuery,
    type DataRevision,
    type PersistentDataStore,
    type PersistentRevisionLease,
    type PersistentRoot,
    type PluginStorageCatalog,
    type PresetCatalog,
    type Versioned,
    type WorkingSetCommit,
} from './persistentDataStore'

const MAX_STAGED_CHARACTER_COUNT = 16
const MAX_STAGED_CHARACTER_BYTES = 4 * 1024 * 1024
const textEncoder = new TextEncoder()

interface NativeStoreError {
    code?: string
    message?: string
    expected?: number
    actual?: number
}

function restoreStoreError(error: unknown): unknown {
    if (error instanceof Error) return error

    let nativeError = error
    if (typeof nativeError === 'string') {
        try {
            nativeError = JSON.parse(nativeError)
        } catch {
            return new Error(String(nativeError))
        }
    }
    if (!nativeError || typeof nativeError !== 'object') return error

    const { code, message, expected, actual } = nativeError as NativeStoreError
    if (code === 'revision-conflict' && expected !== undefined && actual !== undefined) {
        return new RevisionConflictError(expected, actual)
    }
    if (code === 'snapshot-released') return new SnapshotReleasedError()
    if ((code === 'validation' || code === 'store-error') && message !== undefined) {
        return new Error(message)
    }
    return error
}

async function invokeStore<T>(command: string, args?: Record<string, unknown>): Promise<T> {
    try {
        return args === undefined ? await invoke<T>(command) : await invoke<T>(command, args)
    } catch (error) {
        throw restoreStoreError(error)
    }
}

function characterBatches(
    characters: Database['characters'],
): Array<Database['characters']> {
    const batches: Array<Database['characters']> = []
    let batch: Database['characters'] = []
    let batchBytes = 2

    for (const character of characters) {
        const characterBytes = textEncoder.encode(JSON.stringify(character)).byteLength
        const separatorBytes = batch.length === 0 ? 0 : 1
        if (
            batch.length > 0 &&
            (batch.length >= MAX_STAGED_CHARACTER_COUNT ||
                batchBytes + separatorBytes + characterBytes > MAX_STAGED_CHARACTER_BYTES)
        ) {
            batches.push(batch)
            batch = []
            batchBytes = 2
        }
        batchBytes += (batch.length === 0 ? 0 : 1) + characterBytes
        batch.push(character)
    }

    if (batch.length > 0) batches.push(batch)
    return batches
}

export class SqlitePersistentDataStore implements PersistentDataStore {
    async open(): Promise<void> {
        await invokeStore<{ revision: DataRevision }>('pds_open')
    }

    readRoot(): Promise<Versioned<PersistentRoot>> {
        return invokeStore('pds_read_root', {})
    }

    queryPresets(): Promise<PresetCatalog> {
        return invokeStore('pds_query_presets', {})
    }

    readPreset(id: string): Promise<Versioned<botPreset> | null> {
        return invokeStore('pds_read_preset', { id })
    }

    queryCharacters(input: CharacterQuery): Promise<CharacterPage> {
        return invokeStore('pds_query_characters', { query: input })
    }

    readCharacter(id: string): Promise<Versioned<CharacterDetail> | null> {
        return invokeStore('pds_read_character', { id })
    }

    queryConversations(input: ConversationQuery): Promise<ConversationPage> {
        return invokeStore('pds_query_conversations', { query: input })
    }

    readConversation(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<Chat> | null> {
        return invokeStore('pds_read_conversation', { characterId, conversationId })
    }

    async readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null> {
        validateConversationWindowQuery(input)
        return await invokeStore('pds_read_conversation_window', { query: input })
    }

    queryPluginStorage(): Promise<PluginStorageCatalog> {
        return invokeStore('pds_query_plugin_storage', {})
    }

    readPluginStorage(key: string): Promise<Versioned<unknown> | null> {
        return invokeStore('pds_read_plugin_storage', { key })
    }

    readAssetAlias(key: string): Promise<Versioned<AssetAlias> | null> {
        return invokeStore('pds_read_asset_alias', { kind: 'asset', key })
    }

    readAssetOwnerHead(
        owner: AssetOwnerLocator,
    ): Promise<Versioned<AssetOwnerHead> | null> {
        return invokeStore('pds_read_asset_owner_head', { owner })
    }

    commitAssetAlias(
        alias: AssetAlias,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        return invokeStore('pds_commit_asset_alias', { alias, expectedRevision })
    }

    commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }> {
        return invokeStore('pds_commit', { commit: input })
    }

    async replaceFromDatabase(
        database: Database,
        expectedRevision?: DataRevision,
        assetAliases: AssetAlias[] = [],
    ): Promise<{ revision: DataRevision }> {
        const { stagingId } = await invokeStore<{ stagingId: string }>('pds_replace_begin')
        try {
            const { characters, botPresets, ...root } = database
            await invokeStore<void>('pds_replace_put_root', { stagingId, root })
            await invokeStore<void>('pds_replace_put_presets', {
                stagingId,
                presets: botPresets ?? [],
            })
            for (const batch of characterBatches(characters)) {
                await invokeStore<void>('pds_replace_add_characters', {
                    stagingId,
                    characters: batch,
                })
            }
            if (assetAliases.length > 0) {
                await invokeStore<void>('pds_replace_put_asset_aliases', {
                    stagingId,
                    aliases: assetAliases,
                })
            }
            return await invokeStore('pds_replace_commit', {
                stagingId,
                ...(expectedRevision === undefined ? {} : { expectedRevision }),
            })
        } catch (error) {
            try {
                await invokeStore<void>('pds_replace_abort', { stagingId })
            } catch {}
            throw error
        }
    }

    materializeDatabase(revision?: DataRevision): Promise<Database> {
        return revision === undefined
            ? invokeStore('pds_materialize', {})
            : invokeStore('pds_materialize', { revision })
    }

    async acquireRevision(revision: DataRevision): Promise<PersistentRevisionLease> {
        const { lease } = await invokeStore<{ lease: string }>('pds_acquire_revision', {
            revision,
        })
        let released = false
        let releasePromise: Promise<void> | undefined
        const assertActive = () => {
            if (released) throw new SnapshotReleasedError()
        }

        const revisionLease: NativePersistentRevisionLease = {
            revision,
            [nativePersistentRevisionLease]: lease,
            readRoot: async () => {
                assertActive()
                return invokeStore('pds_read_root', { lease })
            },
            queryPresets: async () => {
                assertActive()
                return invokeStore('pds_query_presets', { lease })
            },
            readPreset: async (id) => {
                assertActive()
                return invokeStore('pds_read_preset', { id, lease })
            },
            queryCharacters: async (input) => {
                assertActive()
                return invokeStore('pds_query_characters', { query: input, lease })
            },
            readCharacter: async (id) => {
                assertActive()
                return invokeStore('pds_read_character', { id, lease })
            },
            queryConversations: async (input) => {
                assertActive()
                return invokeStore('pds_query_conversations', { query: input, lease })
            },
            readConversation: async (characterId, conversationId) => {
                assertActive()
                return invokeStore('pds_read_conversation', {
                    characterId,
                    conversationId,
                    lease,
                })
            },
            readConversationWindow: async (input) => {
                assertActive()
                validateConversationWindowQuery(input)
                return invokeStore('pds_read_conversation_window', { query: input, lease })
            },
            queryPluginStorage: async () => {
                assertActive()
                return invokeStore('pds_query_plugin_storage', { lease })
            },
            readPluginStorage: async (key) => {
                assertActive()
                return invokeStore('pds_read_plugin_storage', { key, lease })
            },
            readAssetAlias: async (key) => {
                assertActive()
                return invokeStore('pds_read_asset_alias', { kind: 'asset', key, lease })
            },
            readAssetOwnerHead: async (owner) => {
                assertActive()
                return invokeStore('pds_read_asset_owner_head', { owner, lease })
            },
            release: () => {
                if (releasePromise) return releasePromise
                releasePromise = invokeStore<void>('pds_release_revision', { lease }).then(
                    () => {
                        released = true
                    },
                    (error) => {
                        releasePromise = undefined
                        throw error
                    },
                )
                return releasePromise
            },
        }
        return revisionLease
    }
}
