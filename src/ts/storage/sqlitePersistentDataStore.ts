import { invoke } from '@tauri-apps/api/core'
import { nativeCommitTransport } from './nativeCommitTransport'

import type { Chat, Database, botPreset } from './database.svelte'
import {
    nativePersistentRevisionLease,
    type NativePersistentRevisionLease,
} from './nativePersistentExport'
import {
    RevisionConflictError,
    SnapshotReleasedError,
    validateConversationWindowQuery,
    type ArchivePreview,
    type AssetAlias,
    type AssetAliasIdentity,
    type AssetAliasKind,
    type AssetAliasListQuery,
    type AssetAliasPage,
    type AssetOwnerHead,
    type AssetOwnerLocator,
    type CharacterDetail,
    type CharacterPage,
    type CharacterQuery,
    type CharacterSummary,
    type ContentChangeKey,
    type ContentChangeWindow,
    type ConversationPage,
    type ConversationQuery,
    type ConversationWindow,
    type ConversationWindowQuery,
    type DataRevision,
    type AssetRepositoryAuthorityState,
    type AssetRepositoryMigrationInput,
    type PersistentDataStore,
    type PersistentConversationMetadata,
    type PersistentRevisionLease,
    type PersistentRoot,
    type PluginStorageCatalog,
    type PluginStorageListItem,
    type PluginStorageValue,
    type PresetCatalog,
    type Versioned,
    type WorkingSetCommit,
} from './persistentDataStore'

const MAX_STAGED_CHARACTER_COUNT = 16
const MAX_STAGED_CHARACTER_BYTES = 4 * 1024 * 1024
const MAX_STAGED_ASSET_RECORDS = 512
const textEncoder = new TextEncoder()

/** Mirrors the Rust `PersistentStoreOpenResult` returned by the `pds_open` command. */
export interface PersistentStoreOpenResult {
    revision: DataRevision
    /** Present when a requested snapshot restore was skipped and the old database stayed active. */
    restoreFailure?: string
}

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

function batches<T>(values: T[], limit: number): T[][] {
    const output: T[][] = []
    for (let index = 0; index < values.length; index += limit) {
        output.push(values.slice(index, index + limit))
    }
    return output
}

export class SqlitePersistentDataStore implements PersistentDataStore {
    /** Latest native revision and skipped restore details; every open still reaches the store. */
    lastOpenResult: PersistentStoreOpenResult | null = null

    async open(): Promise<void> {
        const result = await invokeStore<PersistentStoreOpenResult>('pds_open')
        this.lastOpenResult = result
        if (result?.restoreFailure) {
            // The native side already wrote this to the native log, but the web layer keeps its
            // own console breadcrumb so a skipped restore is not silent in the frontend.
            console.warn(
                `Persistent store opened without the requested snapshot restore: ${result.restoreFailure}`,
            )
        }
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

    readCharacterSummary(id: string): Promise<CharacterSummary | null> {
        return invokeStore('pds_read_character_summary', { id })
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

    readConversationMetadata(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<PersistentConversationMetadata> | null> {
        return invokeStore('pds_read_conversation_metadata', {
            characterId,
            conversationId,
        })
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

    readPluginStorage(owner: string, key: string): Promise<Versioned<unknown> | null> {
        return invokeStore('pds_read_plugin_storage', { owner, key })
    }

    listPluginStorage(): Promise<PluginStorageListItem[]> {
        return invokeStore('pds_list_plugin_storage', {})
    }

    commitWorkingSetChangeCursor(revision: DataRevision): Promise<void> {
        return invokeStore('pds_commit_working_set_change_cursor', { revision })
    }

    readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null> {
        return invokeStore('pds_read_asset_alias', { ...identity })
    }

    readAssetAliasesByKeys(
        kind: AssetAliasKind,
        keys: string[],
    ): Promise<Versioned<AssetAlias[]>> {
        return invokeStore('pds_read_asset_aliases_by_keys', { kind, keys })
    }

    listAssetAliases(query: AssetAliasListQuery): Promise<AssetAliasPage> {
        return invokeStore('pds_list_asset_aliases', { query })
    }

    readAssetRepositoryAuthority(): Promise<Versioned<AssetRepositoryAuthorityState>> {
        return invokeStore('pds_read_asset_repository_authority', {})
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

    deleteAssetAlias(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        return invokeStore('pds_delete_asset_alias', { ...identity, expectedRevision })
    }

    async activateAssetRepositoryMigration(
        input: AssetRepositoryMigrationInput,
    ): Promise<{ revision: DataRevision }> {
        const { stagingId } = await invokeStore<{ stagingId: string }>('pds_replace_begin')
        try {
            await invokeStore<void>('pds_replace_put_asset_repository_authority', {
                stagingId,
                authority: {
                    format: 'preparing',
                    migrationId: input.migrationId,
                    sourceRevision: input.sourceRevision,
                },
            })
            const { characters, botPresets, ...root } = input.database
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
            for (const aliases of batches(input.assetAliases, MAX_STAGED_ASSET_RECORDS)) {
                await invokeStore<void>('pds_replace_put_asset_aliases', {
                    stagingId,
                    aliases,
                })
            }
            for (const heads of batches(input.assetOwnerHeads, MAX_STAGED_ASSET_RECORDS)) {
                await invokeStore<void>('pds_replace_put_asset_owner_heads', {
                    stagingId,
                    heads,
                })
            }
            await invokeStore<void>('pds_replace_put_asset_repository_authority', {
                stagingId,
                authority: {
                    format: 'v2',
                    migrationId: input.migrationId,
                    compatibilityHash: input.compatibilityHash,
                },
            })
            return await invokeStore('pds_replace_commit', {
                stagingId,
                expectedRevision: input.sourceRevision,
            })
        } catch (error) {
            try {
                await invokeStore<void>('pds_replace_abort', { stagingId })
            } catch {}
            throw error
        }
    }

    commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }> {
        const { assetAliases = [], ...commit } = input
        return nativeCommitTransport.commit({ commit, assetAliases }).catch((error) => {
            throw restoreStoreError(error)
        })
    }

    archivePreview(characterId: string): Promise<ArchivePreview> {
        return invokeStore('pds_archive_preview', { characterId })
    }

    archiveCharacter(
        characterId: string,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        return invokeStore('pds_archive_character', { characterId, expectedRevision })
    }

    restoreCharacter(
        characterId: string,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        return invokeStore('pds_restore_character', { characterId, expectedRevision })
    }

    async replaceFromDatabase(
        database: Database,
        expectedRevision?: DataRevision,
        assetAliases: AssetAlias[] = [],
        pluginStorageValues?: PluginStorageValue[],
    ): Promise<{ revision: DataRevision }> {
        const { stagingId } = await invokeStore<{ stagingId: string }>('pds_replace_begin')
        try {
            const { characters, botPresets, ...root } = database
            await invokeStore<void>('pds_replace_put_root', {
                stagingId,
                root,
                ...(pluginStorageValues ? { pluginStorageValues } : {}),
            })
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
            const preserved = await invokeStore<{ revision: DataRevision }>(
                'pds_replace_preserve_repositories',
                {
                    stagingId,
                    ...(expectedRevision === undefined ? {} : { expectedRevision }),
                },
            )
            for (const aliases of batches(assetAliases, MAX_STAGED_ASSET_RECORDS)) {
                await invokeStore<void>('pds_replace_put_asset_aliases', {
                    stagingId,
                    aliases,
                })
            }
            return await invokeStore('pds_replace_commit', {
                stagingId,
                expectedRevision: preserved.revision,
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
            readCharacterSummary: async (id) => {
                assertActive()
                return invokeStore('pds_read_character_summary', { id, lease })
            },
            readCharacter: async (id) => {
                assertActive()
                return invokeStore('pds_read_character', { id, lease })
            },
            readWorkingSetChangeWindow: async () => {
                assertActive()
                return invokeStore<ContentChangeWindow>('pds_working_set_change_window', {
                    lease,
                })
            },
            readWorkingSetChangePage: async (afterRevision, afterKey, limit) => {
                assertActive()
                return invokeStore<ContentChangeKey[]>('pds_working_set_change_page', {
                    lease,
                    afterRevision,
                    afterKey,
                    limit,
                })
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
            readConversationMetadata: async (characterId, conversationId) => {
                assertActive()
                return invokeStore('pds_read_conversation_metadata', {
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
            readPluginStorage: async (owner, key) => {
                assertActive()
                return invokeStore('pds_read_plugin_storage', { owner, key, lease })
            },
            readAssetAlias: async (identity) => {
                assertActive()
                return invokeStore('pds_read_asset_alias', { ...identity, lease })
            },
            readAssetAliasesByKeys: async (kind, keys) => {
                assertActive()
                return invokeStore('pds_read_asset_aliases_by_keys', { kind, keys, lease })
            },
            listAssetAliases: async (query) => {
                assertActive()
                return invokeStore('pds_list_asset_aliases', { query, lease })
            },
            readAssetRepositoryAuthority: async () => {
                assertActive()
                return invokeStore('pds_read_asset_repository_authority', { lease })
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
