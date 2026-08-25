import type { Database } from './database.svelte'
import {
    hasNativePersistentRevisionLease,
    type NativePersistentExportFile,
    type NativePersistentExportOptions,
    withPinnedNativePersistentRisuSaveFile,
} from './nativePersistentExport'
import type {
    CharacterSummary,
    DataRevision,
    PersistentDataStore,
    PersistentRevisionLease,
} from './persistentDataStore'
import {
    replaceCharacterResources,
    replaceDatabaseRootResources,
} from '../process/coldstorageData'
import {
    decodeRisuSave,
    encodeRisuSaveBlock,
    magicRisuSaveHeader,
    RisuSaveType,
} from './risuSave'

export async function importRisuSaveToStore(
    bytes: Uint8Array,
    store: PersistentDataStore,
): Promise<{ revision: DataRevision }> {
    return store.replaceFromDatabase((await decodeRisuSave(bytes)) as Database)
}

async function characterSummaries(lease: PersistentRevisionLease): Promise<CharacterSummary[]> {
    const summaries: CharacterSummary[] = []
    for (const trash of [false, true]) {
        let cursor: string | undefined
        do {
            const page = await lease.queryCharacters({
                order: 'configured',
                trash,
                limit: 128,
                cursor,
            })
            summaries.push(...page.items)
            cursor = page.nextCursor
        } while (cursor !== undefined)
    }
    return summaries.sort((left, right) => left.configuredIndex - right.configuredIndex)
}

async function* characterValues(lease: PersistentRevisionLease): AsyncGenerator<Database['characters'][number]> {
    for (const summary of await characterSummaries(lease)) {
        const detail = await lease.readCharacter(summary.id)
        if (!detail) throw new Error(`Missing character detail for ${summary.id}`)
        const chats: Database['characters'][number]['chats'] = []
        let conversationCursor: string | undefined
        do {
            const conversations = await lease.queryConversations({
                characterId: summary.id,
                order: 'configured',
                limit: 128,
                cursor: conversationCursor,
            })
            for (const conversation of conversations.items) {
                const storedConversation = await lease.readConversation(
                    summary.id,
                    conversation.id,
                )
                if (!storedConversation) {
                    throw new Error(`Missing conversation ${conversation.id}`)
                }
                chats.push(storedConversation.value)
            }
            conversationCursor = conversations.nextCursor
        } while (conversationCursor !== undefined)
        yield { ...detail.value, chats } as Database['characters'][number]
    }
}

async function releaseRevisionLease(lease: PersistentRevisionLease): Promise<void> {
    try {
        await lease.release()
    } catch (firstError) {
        try {
            await lease.release()
        } catch {
            throw firstError
        }
    }
}

export async function* streamRisuSaveFromStore(
    store: PersistentDataStore,
    revision: DataRevision,
    options?: RisuSaveStreamOptions,
): AsyncGenerator<Uint8Array> {
    const lease = await store.acquireRevision(revision)
    let exportFailed = false
    try {
        yield* streamRisuSaveFromLease(lease, options)
    } catch (error) {
        exportFailed = true
        throw error
    } finally {
        try {
            await releaseRevisionLease(lease)
        } catch (error) {
            if (!exportFailed) throw error
        }
    }
}

export interface RisuSaveStreamOptions {
    replaceResources?: Readonly<Record<string, string>>
    omitAccount?: boolean
}

async function presetValues(lease: PersistentRevisionLease): Promise<Database['botPresets']> {
    const presets: Database['botPresets'] = []
    const catalog = await lease.queryPresets()
    for (const summary of catalog.items) {
        const preset = await lease.readPreset(summary.id)
        if (!preset) throw new Error(`Missing preset ${summary.id}`)
        presets.push(preset.value)
    }
    return presets
}

export async function* streamRisuSaveFromLease(
    lease: PersistentRevisionLease,
    options?: RisuSaveStreamOptions,
): AsyncGenerator<Uint8Array> {
    const storedRoot = (await lease.readRoot()).value
    const storedPresets = await presetValues(lease)
    const rootWithPresets = { ...storedRoot, botPresets: storedPresets } as Database
    const root = options?.replaceResources
        ? replaceDatabaseRootResources(rootWithPresets, options.replaceResources)
        : rootWithPresets
    const directory: string[] = [
        'preset',
        'modules',
        'loadouts',
        'plugins',
        'pluginStorage',
    ]
    const characterIds = (await characterSummaries(lease)).map((item) => item.id)
    directory.push(...characterIds, 'config')

    const {
        botPresets,
        modules,
        loadouts,
        plugins,
        pluginCustomStorage,
        ...rootData
    } = root
    const exportedRoot = options?.omitAccount
        ? Object.fromEntries(Object.entries(rootData).filter(([key]) => key !== 'account'))
        : rootData
    yield magicRisuSaveHeader.slice()
    yield await encodeRisuSaveBlock({
        compression: true,
        data: JSON.stringify({ ...exportedRoot, __directory: directory }),
        type: RisuSaveType.ROOT,
        name: 'root',
    })
    for (const [type, name, value] of [
        [RisuSaveType.BOTPRESET, 'preset', botPresets],
        [RisuSaveType.MODULES, 'modules', modules],
        [RisuSaveType.LOADOUTS, 'loadouts', loadouts],
        [RisuSaveType.PLUGINS, 'plugins', plugins],
        [RisuSaveType.PLUGIN_STORAGE, 'pluginStorage', pluginCustomStorage],
    ] as const) {
        yield await encodeRisuSaveBlock({
            compression: true,
            data: JSON.stringify(value),
            type,
            name,
        })
    }
    for await (const storedCharacter of characterValues(lease)) {
        const character = options?.replaceResources
            ? replaceCharacterResources(storedCharacter, options.replaceResources)
            : storedCharacter
        yield await encodeRisuSaveBlock({
            compression: true,
            data: JSON.stringify(character),
            type: RisuSaveType.CHARACTER_WITH_CHAT,
            name: character.chaId,
        })
    }
    yield await encodeRisuSaveBlock({
        compression: true,
        data: JSON.stringify({ version: 1 }),
        type: RisuSaveType.CONFIG,
        name: 'config',
    })
}

export interface RisuSaveExportRuntime {
    readonly store: PersistentDataStore
    capturePersistentMutationToken(reason: string): Promise<{
        revision: DataRevision
        mutationGeneration: number
    }>
}

export interface PinnedRisuSaveExport {
    readonly revision: DataRevision
    readonly mutationGeneration: number
    countCharacters(): Promise<number>
    materializeDatabase(): Promise<Database>
    stream(options?: RisuSaveStreamOptions): AsyncGenerator<Uint8Array>
    collectBytes(options?: RisuSaveStreamOptions): Promise<Uint8Array>
    withNativeFile?<T>(
        options: NativePersistentExportOptions,
        callback: (file: NativePersistentExportFile) => Promise<T>,
    ): Promise<T>
}

async function materializeDatabaseFromLease(
    lease: PersistentRevisionLease,
): Promise<Database> {
    const root = (await lease.readRoot()).value
    const botPresets = await presetValues(lease)
    const characters: Database['characters'] = []
    for await (const character of characterValues(lease)) {
        characters.push(character)
    }
    return { ...root, characters, botPresets } as Database
}

async function collectChunks(chunks: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
    const values: Uint8Array[] = []
    let length = 0
    for await (const chunk of chunks) {
        values.push(chunk)
        length += chunk.byteLength
    }
    const result = new Uint8Array(length)
    let offset = 0
    for (const value of values) {
        result.set(value, offset)
        offset += value.byteLength
    }
    return result
}

export async function withFlushedRisuSaveExport<T>(
    runtime: RisuSaveExportRuntime,
    reason: string,
    callback: (pinned: PinnedRisuSaveExport) => Promise<T>,
): Promise<T> {
    const token = await runtime.capturePersistentMutationToken(reason)
    const lease = await runtime.store.acquireRevision(token.revision)
    const pinned: PinnedRisuSaveExport = {
        revision: token.revision,
        mutationGeneration: token.mutationGeneration,
        countCharacters: async () => (await characterSummaries(lease)).length,
        materializeDatabase: () => materializeDatabaseFromLease(lease),
        stream: (options) => streamRisuSaveFromLease(lease, options),
        collectBytes: (options) => collectChunks(streamRisuSaveFromLease(lease, options)),
        ...(hasNativePersistentRevisionLease(lease)
            ? {
                  withNativeFile: <T>(
                      options: NativePersistentExportOptions,
                      nativeCallback: (file: NativePersistentExportFile) => Promise<T>,
                  ) => withPinnedNativePersistentRisuSaveFile(
                      lease,
                      options,
                      nativeCallback,
                  ),
              }
            : {}),
    }
    let exportFailed = false
    try {
        return await callback(pinned)
    } catch (error) {
        exportFailed = true
        throw error
    } finally {
        try {
            await releaseRevisionLease(lease)
        } catch (error) {
            if (!exportFailed) throw error
        }
    }
}
