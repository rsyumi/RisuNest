import { stableSelectionRoot, projectSelectionIndexes } from './persistentSelectionBoundary'
import { jsonByteLength, prepareNativePersistenceValue, type PayloadTooLargeKind } from './nativePersistenceValue'
import { invoke } from '@tauri-apps/api/core'
import { nativeCommitTransport, STAGED_REQUEST_BYTES, type NativeCommitTransport } from './nativeCommitTransport'
import { rendererDocument } from './rendererDocument'

import type { Chat, Database, botPreset } from './database.svelte'
import {
    nativePersistentRevisionLease,
    type NativePersistentRevisionLease,
} from './nativePersistentExport'
import {
    RevisionConflictError,
    SnapshotReleasedError,
    validateConversationWindowQuery,
    validatePluginStorageValueQuery,
    type PluginStorageValueQuery,
    type PluginStorageValuePage,
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
    type AuthorNotePresetUsage,
    type ConversationQuery,
    type ConversationMessageMetadataWindow,
    type ConversationWindow,
    type ConversationWindowQuery,
    type DataRevision,
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
    type LwwStageReceive,
    type LwwOutboxRequest, type LwwOutboxPage, type LwwReplacementRequest,
    type LwwApplyReceive,
    type LwwApplyResult,
    type LwwReceiveHeader,
} from './persistentDataStore'

const MAX_STAGED_CHARACTER_COUNT = 16
const MAX_STAGED_ASSET_RECORDS = 512

/** Mirrors the Rust `PersistentStoreOpenResult` returned by the `pds_open` command. */
export interface PersistentStoreOpenResult {
    revision: DataRevision
    /** Present when a requested snapshot restore was skipped and the old database stayed active. */
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
    if ((code === 'validation' || code === 'store-error' || code === 'commit-decode' || code === 'schema-mismatch') && message !== undefined) {
        return Object.assign(new Error(message), { code })
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

async function invokeArchiveOperation<T>(
    command: 'pds_archive_character' | 'pds_restore_character',
    args: Record<string, unknown>,
    signal?: AbortSignal,
): Promise<T> {
    if (signal?.aborted) {
        throw new DOMException('Character archive operation was cancelled', 'AbortError')
    }
    const operationId = `character-archive-${globalThis.crypto.randomUUID()}`
    const cancel = () => {
        void invokeStore('pds_cancel_character_archive_operation', { operationId }).catch(() => {})
    }
    signal?.addEventListener('abort', cancel, { once: true })
    try {
        return await invokeStore<T>(command, { ...args, operationId })
    } catch (error) {
        if (signal?.aborted) {
            throw new DOMException('Character archive operation was cancelled', 'AbortError')
        }
        throw error
    } finally {
        signal?.removeEventListener('abort', cancel)
    }
}

type StageRequest = (command: string, kind: PayloadTooLargeKind, args: Record<string, unknown>) => Promise<void>
type Character = Database['characters'][number]

const isRecord = (value: unknown): value is Record<string, unknown> =>
    typeof value === 'object' && value !== null && !Array.isArray(value)

/** Batches of at most the staged request budget; a larger item goes alone. */
function byteBatches<T>(
    items: T[],
    size: (item: T) => number = jsonByteLength,
    maxCount = Infinity,
): T[][] {
    const output: T[][] = []
    let batch: T[] = []
    let batchBytes = 2
    for (const item of items) {
        const itemBytes = size(item)
        if (batch.length > 0 && (batch.length >= maxCount || batchBytes + 1 + itemBytes > STAGED_REQUEST_BYTES)) {
            output.push(batch)
            batch = []
            batchBytes = 2
        }
        batchBytes += (batch.length === 0 ? 0 : 1) + itemBytes
        batch.push(item)
    }
    if (batch.length > 0) output.push(batch)
    return output
}

// A character above the budget is staged as its detail, then each chat
// without its messages, then those messages in pages.
function characterPieces(characters: Character[]): Array<{ batch: Character[] } | { character: Character }> {
    const pieces: Array<{ batch: Character[] } | { character: Character }> = []
    let small: Array<{ character: Character; bytes: number }> = []
    const flush = () => {
        for (const batch of byteBatches(small, (entry) => entry.bytes, MAX_STAGED_CHARACTER_COUNT)) {
            pieces.push({ batch: batch.map((entry) => entry.character) })
        }
        small = []
    }
    for (const character of characters) {
        const bytes = jsonByteLength(character)
        if (bytes <= STAGED_REQUEST_BYTES) {
            small.push({ character, bytes })
            continue
        }
        flush()
        pieces.push({ character })
    }
    flush()
    return pieces
}

async function stageCharacterInPieces(stage: StageRequest, character: Character): Promise<void> {
    const { chats, ...detail } = character
    const conversations: unknown[] = Array.isArray(chats) ? chats : []
    await stage('pds_replace_put_character_detail', 'character', { detail, conversationCount: conversations.length })
    for (const [configuredIndex, chat] of conversations.entries()) {
        const messages: unknown[] = isRecord(chat) && Array.isArray(chat.message) ? chat.message : []
        let conversation = chat
        if (isRecord(chat)) {
            const { message: _message, ...rest } = chat
            conversation = rest
        }
        const last = messages.at(-1)
        await stage('pds_replace_put_conversation', 'conversation', {
            characterId: character.chaId,
            configuredIndex,
            conversation,
            messageCount: messages.length,
            ...(isRecord(last) && last.time !== undefined ? { lastMessageTime: last.time } : {}),
        })
        let start = 0
        for (const page of byteBatches(messages)) {
            await stage('pds_replace_add_conversation_messages', 'message', {
                characterId: character.chaId,
                conversationId: isRecord(conversation) ? conversation.id : undefined,
                start,
                messages: page,
            })
            start += page.length
        }
    }
}

// Plugin values leave the root request when the two exceed the budget.
async function stageRoot(
    stage: StageRequest,
    root: Record<string, unknown>,
    pluginStorageValues?: PluginStorageValue[],
): Promise<void> {
    if (pluginStorageValues) {
        const split = jsonByteLength(root) + jsonByteLength(pluginStorageValues) > STAGED_REQUEST_BYTES
        await stage('pds_replace_put_root', 'root', { root, pluginStorageValues: split ? [] : pluginStorageValues })
        if (!split) return
        for (const values of byteBatches(pluginStorageValues)) {
            await stage('pds_replace_add_plugin_storage_values', 'plugin-value', { values })
        }
        return
    }
    const { pluginCustomStorage: storage, pluginStorageMeta: meta, ...rest } = root
    if (!isRecord(storage) || jsonByteLength(root) <= STAGED_REQUEST_BYTES) {
        await stage('pds_replace_put_root', 'root', { root })
        return
    }
    await stage('pds_replace_put_root', 'root', { root: rest })
    const owners = isRecord(meta) ? meta : undefined
    const owned = (key: string) => owners !== undefined && Object.hasOwn(owners, key)
    const size = (key: string) => jsonByteLength(key) + jsonByteLength(storage[key])
        + (owned(key) ? jsonByteLength(key) + jsonByteLength(owners![key]) : 0)
    for (const keys of byteBatches(Object.keys(storage), size)) {
        await stage('pds_replace_add_plugin_storage', 'plugin-value', {
            storage: Object.fromEntries(keys.map((key) => [key, storage[key]])),
            ...(owners ? { meta: Object.fromEntries(keys.filter(owned).map((key) => [key, owners[key]])) } : {}),
        })
    }
}

async function stagePresets(stage: StageRequest, presets: botPreset[]): Promise<void> {
    const [first = [], ...rest] = byteBatches(presets)
    await stage('pds_replace_put_presets', 'preset', { presets: first })
    for (const batch of rest) await stage('pds_replace_add_presets', 'preset', { presets: batch })
}

function batches<T>(values: T[], limit: number): T[][] {
    const output: T[][] = []
    for (let index = 0; index < values.length; index += limit) {
        output.push(values.slice(index, index + limit))
    }
    return output
}

export class SqlitePersistentDataStore implements PersistentDataStore {
    readonly messageMetadataReadMode = 'projected' as const
    /** Latest native revision; every open still reaches the store. */
    lastOpenResult: PersistentStoreOpenResult | null = null

    constructor(private readonly transport: NativeCommitTransport = nativeCommitTransport) {}

    async open(): Promise<void> {
        const result = await invokeStore<PersistentStoreOpenResult>('pds_open', { document: rendererDocument() })
        this.lastOpenResult = result
    }

    readRoot(): Promise<Versioned<PersistentRoot>> {
        return invokeStore('pds_read_root', {})
    }

    readRevision(): Promise<DataRevision> {
        return invokeStore('pds_read_revision', {})
    }

    readLibraryIdentity(): Promise<string> {
        return invokeStore('pds_read_library_identity', {})
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

    queryAuthorNotePresetUsage(presetId: string): Promise<AuthorNotePresetUsage[]> {
        return invokeStore('pds_query_author_note_preset_usage', { presetId })
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

    async readConversationMessageMetadataWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationMessageMetadataWindow> | null> {
        validateConversationWindowQuery(input)
        return await invokeStore('pds_read_conversation_message_metadata_window', {
            query: input,
        })
    }

    readPluginStorageValues(query: PluginStorageValueQuery): Promise<PluginStorageValuePage> {
        validatePluginStorageValueQuery(query)
        return invokeStore('pds_read_plugin_storage_page', { query })
    }

    queryPluginStorage(owner?: string): Promise<PluginStorageCatalog> {
        return invokeStore('pds_query_plugin_storage', owner === undefined ? {} : { owner })
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

    readAssetOwnerHead(
        owner: AssetOwnerLocator,
    ): Promise<Versioned<AssetOwnerHead> | null> {
        return invokeStore('pds_read_asset_owner_head', { owner })
    }

    commitAssetAlias(
        alias: AssetAlias,
        expectedRevision: DataRevision,
        expectedLibrary?: string,
    ): Promise<{ revision: DataRevision }> {
        return invokeStore('pds_commit_asset_alias', { alias, expectedRevision, ...(expectedLibrary === undefined ? {} : { expectedLibrary }) })
    }

    deleteAssetAlias(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        return invokeStore('pds_delete_asset_alias', { ...identity, expectedRevision })
    }

    commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }> {
        const { assetAliases = [], ...commit } = input
        return this.transport.commit({ commit, assetAliases }).catch((error) => {
            throw restoreStoreError(error)
        })
    }

    lwwBindingState(): Promise<{ targetAuthority: string }> { return invokeStore('pds_lww_binding_state', {}) }
    lwwReadOutbox(request: LwwOutboxRequest): Promise<LwwOutboxPage> { return invokeStore('pds_lww_read_outbox', { request }) }
    lwwCommitReplacement(request: LwwReplacementRequest): Promise<{ revision: DataRevision }> { return invokeStore('pds_lww_commit_replacement', { request }) }

    lwwStageReceive(request: LwwStageReceive): Promise<void> {
        return invokeStore('pds_lww_stage_receive', { request })
    }

    lwwApplyReceive(request: LwwApplyReceive): Promise<LwwApplyResult> {
        return invokeStore('pds_lww_apply_receive', { request })
    }

    lwwFinishReceive(request: LwwReceiveHeader): Promise<void> {
        return invokeStore('pds_lww_finish_receive', { request })
    }

    lwwDrainDeferred(request: LwwApplyReceive): Promise<LwwApplyResult> {
        return invokeStore('pds_lww_drain_deferred', { request })
    }

    archivePreview(characterId: string): Promise<ArchivePreview> {
        return invokeStore('pds_archive_preview', { characterId })
    }

    archiveCharacter(
        characterId: string,
        expectedRevision: DataRevision,
        signal?: AbortSignal,
    ): Promise<{ revision: DataRevision }> {
        return invokeArchiveOperation(
            'pds_archive_character',
            { characterId, expectedRevision },
            signal,
        )
    }

    restoreCharacter(
        characterId: string,
        expectedRevision: DataRevision,
        signal?: AbortSignal,
    ): Promise<{ revision: DataRevision }> {
        return invokeArchiveOperation(
            'pds_restore_character',
            { characterId, expectedRevision },
            signal,
        )
    }

    async replaceFromDatabase(...args: Parameters<PersistentDataStore['replaceFromDatabase']>): Promise<{revision: DataRevision}> {
        const stage = await this.stageDatabaseReplacement(...args)
        try { return await stage.activate() }
        catch (error) { try { await stage.abort() } catch {} ; throw error }
    }

    async stageDatabaseReplacement(
        database: Database,
        expectedRevision?: DataRevision,
        assetAliases: AssetAlias[] = [],
        pluginStorageValues?: PluginStorageValue[],
        replacementHeader?: import('./persistentDataStore').LwwReceiveHeader,
    ): Promise<import('./persistentDataStore').PersistentDatabaseReplacementStage> {
        replacementHeader = replacementHeader && {...replacementHeader}
        database = prepareNativePersistenceValue(database, 'replacement database')
        assetAliases = prepareNativePersistenceValue(assetAliases, 'asset aliases')
        pluginStorageValues = prepareNativePersistenceValue(pluginStorageValues, 'plugin storage')
        const { stagingId } = await invokeStore<{ stagingId: string }>('pds_replace_begin')
        try {
            const { characters, botPresets } = database
            const stage: StageRequest = async (command, kind, args) => {
                try { await this.transport.stage(command, kind, { stagingId, ...args }) }
                catch (error) { throw restoreStoreError(error) }
            }
            await stageRoot(stage, stableSelectionRoot(database) as Record<string, unknown>, pluginStorageValues)
            await stagePresets(stage, botPresets ?? [])
            for (const piece of characterPieces(characters)) {
                if ('character' in piece) await stageCharacterInPieces(stage, piece.character)
                else await stage('pds_replace_add_characters', 'character', { characters: piece.batch })
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
            const header = replacementHeader && {...replacementHeader}
            let submitted = false
            return {
                async activate() {
                    if (submitted) throw new Error('Replacement activation was already submitted')
                    submitted = true
                    if (header) return invokeStore('pds_lww_commit_replacement', {request: {...header, stagingId}})
                    return invokeStore('pds_replace_commit', {stagingId, expectedRevision: preserved.revision})
                },
                async abort() {
                    if (!submitted) await invokeStore<void>('pds_replace_abort', {stagingId})
                },
            }
        } catch (error) {
            try {
                await invokeStore<void>('pds_replace_abort', { stagingId })
            } catch {}
            throw error
        }
    }

    materializeDatabase(revision?: DataRevision): Promise<Database> {
        return (revision === undefined
            ? invokeStore<Database>('pds_materialize', {})
            : invokeStore<Database>('pds_materialize', { revision })).then(projectSelectionIndexes)
    }

    async acquireRevision(revision: DataRevision): Promise<PersistentRevisionLease> {
        const { lease } = await invokeStore<{ lease: string }>('pds_acquire_revision', {
            revision,
            document: rendererDocument(),
        })
        let released = false
        let releasePromise: Promise<void> | undefined
        const assertActive = () => {
            if (released) throw new SnapshotReleasedError()
        }

        const revisionLease: NativePersistentRevisionLease = {
            revision,
            messageMetadataReadMode: 'projected',
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
            readConversationMessageMetadataWindow: async (input) => {
                assertActive()
                validateConversationWindowQuery(input)
                return invokeStore('pds_read_conversation_message_metadata_window', {
                    query: input,
                    lease,
                })
            },
            readPluginStorageValues: async (query) => {
                assertActive()
                validatePluginStorageValueQuery(query)
                return invokeStore('pds_read_plugin_storage_page', { query, lease })
            },
            queryPluginStorage: async (owner) => {
                assertActive()
                return invokeStore('pds_query_plugin_storage', owner === undefined ? { lease } : { lease, owner })
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
