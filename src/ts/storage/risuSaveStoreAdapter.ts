import type { Database } from './database.svelte'
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

export async function* streamRisuSaveFromStore(
    store: PersistentDataStore,
    revision: DataRevision,
    options?: RisuSaveStreamOptions,
): AsyncGenerator<Uint8Array> {
    const lease = await store.acquireRevision(revision)
    try {
        yield* streamRisuSaveFromLease(lease, options)
    } finally {
        await lease.release()
    }
}

export interface RisuSaveStreamOptions {
    replaceResources?: Readonly<Record<string, string>>
}

export async function* streamRisuSaveFromLease(
    lease: PersistentRevisionLease,
    options?: RisuSaveStreamOptions,
): AsyncGenerator<Uint8Array> {
    const storedRoot = (await lease.readRoot()).value
    const root = options?.replaceResources
        ? replaceDatabaseRootResources(storedRoot, options.replaceResources)
        : storedRoot
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
    yield magicRisuSaveHeader.slice()
    yield await encodeRisuSaveBlock({
        compression: true,
        data: JSON.stringify({ ...rootData, __directory: directory }),
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
