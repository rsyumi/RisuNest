import type { Database } from './database.svelte'
import {
    hasNativePersistentRevisionLease,
    type NativePersistentExportFile,
    type NativePersistentExportOptions,
    withPinnedNativePersistentRisuSaveFile,
} from './nativePersistentExport'
import type {
    DataRevision,
    PersistentDataStore,
    PersistentRevisionReader,
} from './persistentDataStore'
import {
    assertPinnedRevision,
    collectPinnedCharacterIds,
    countPinnedCharacters,
    iteratePinnedCharacters,
    iteratePinnedConversations,
    releasePersistentRevisionLease,
} from './persistentRecordIterator'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import {
    replaceCharacterResources,
    replaceDatabaseRootResources,
} from '../process/coldstorageData'
import { replaceExactPluginStorageAssetReferences } from '../drive/backupAssets'
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

async function* characterValues(reader: PersistentRevisionReader): AsyncGenerator<Database['characters'][number]> {
    for await (const record of iteratePinnedCharacters(reader)) {
        const chats: Database['characters'][number]['chats'] = []
        for await (const conversation of iteratePinnedConversations(reader, record.summary.id)) {
            chats.push(conversation.value)
        }
        yield { ...record.detail, chats } as Database['characters'][number]
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
            await releasePersistentRevisionLease(lease)
        } catch (error) {
            if (!exportFailed) throw error
        }
    }
}

export interface RisuSaveStreamOptions {
    replaceResources?: Readonly<Record<string, string>>
    omitAccount?: boolean
}

async function presetValues(reader: PersistentRevisionReader): Promise<Database['botPresets']> {
    const presets: Database['botPresets'] = []
    const catalog = await reader.queryPresets()
    assertPinnedRevision(reader.revision, catalog.revision, 'Preset catalog')
    for (const summary of catalog.items) {
        const preset = await reader.readPreset(summary.id)
        if (!preset) throw new Error(`Missing preset ${summary.id}`)
        assertPinnedRevision(reader.revision, preset.revision, `Preset ${summary.id}`)
        presets.push(preset.value)
    }
    return presets
}

async function pluginStorageValues(
    reader: PersistentRevisionReader,
): Promise<Database['pluginCustomStorage']> {
    const storage: Database['pluginCustomStorage'] = {}
    const catalog = await reader.queryPluginStorage()
    assertPinnedRevision(reader.revision, catalog.revision, 'Plugin storage catalog')
    for (const summary of catalog.items) {
        const value = await reader.readPluginStorage(summary.key)
        if (!value) throw new Error(`Missing plugin storage value for ${summary.key}`)
        assertPinnedRevision(
            reader.revision,
            value.revision,
            `Plugin storage value ${summary.key}`,
        )
        defineOwnEnumerableProperty(storage, summary.key, value.value)
    }
    return storage
}

export async function* streamRisuSaveFromLease(
    reader: PersistentRevisionReader,
    options?: RisuSaveStreamOptions,
): AsyncGenerator<Uint8Array> {
    const storedRootRecord = await reader.readRoot()
    assertPinnedRevision(reader.revision, storedRootRecord.revision, 'Root')
    const storedRoot = storedRootRecord.value
    const storedPresets = await presetValues(reader)
    const storedPluginStorage = await pluginStorageValues(reader)
    const rootWithPresets = {
        ...storedRoot,
        botPresets: storedPresets,
        pluginCustomStorage: storedPluginStorage,
    } as Database
    const root = options?.replaceResources
        ? replaceDatabaseRootResources(rootWithPresets, options.replaceResources)
        : rootWithPresets
    if (options?.replaceResources) {
        root.pluginCustomStorage = replaceExactPluginStorageAssetReferences(
            root.pluginCustomStorage,
            options.replaceResources,
        )
    }
    const directory: string[] = [
        'preset',
        'modules',
        'loadouts',
        'plugins',
        'pluginStorage',
    ]
    const characterIds = await collectPinnedCharacterIds(reader)
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
    for await (const storedCharacter of characterValues(reader)) {
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
    readonly reader: PersistentRevisionReader
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
    reader: PersistentRevisionReader,
): Promise<Database> {
    const rootRecord = await reader.readRoot()
    assertPinnedRevision(reader.revision, rootRecord.revision, 'Root')
    const root = rootRecord.value
    const botPresets = await presetValues(reader)
    const pluginCustomStorage = await pluginStorageValues(reader)
    const characters: Database['characters'] = []
    for await (const character of characterValues(reader)) {
        characters.push(character)
    }
    return { ...root, characters, botPresets, pluginCustomStorage } as Database
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
    return withPinnedRisuSaveExport(runtime.store, token, callback)
}

export async function withPinnedRisuSaveExport<T>(
    store: PersistentDataStore,
    token: { revision: DataRevision; mutationGeneration: number },
    callback: (pinned: PinnedRisuSaveExport) => Promise<T>,
): Promise<T> {
    const lease = await store.acquireRevision(token.revision)
    const pinned: PinnedRisuSaveExport = {
        revision: token.revision,
        mutationGeneration: token.mutationGeneration,
        reader: lease,
        countCharacters: () => countPinnedCharacters(lease),
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
            await releasePersistentRevisionLease(lease)
        } catch (error) {
            if (!exportFailed) throw error
        }
    }
}
