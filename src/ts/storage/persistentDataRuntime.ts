import type { Chat, Database, character, groupChat } from './database.svelte'
import { ActiveWorkingSet } from './activeWorkingSet.svelte'
import type { DataRevision, PersistentDataStore } from './persistentDataStore'
import {
    SaveCoordinator,
    type OfficialRevisionPublisher,
    type SaveCoordinatorClock,
} from './saveCoordinator'
import { streamRisuSaveFromStore } from './risuSaveStoreAdapter'

type CompleteCharacter = character | groupChat
type RootDatabase = Omit<Database, 'characters'>

export function capturePersistentRoot(database: Database): RootDatabase {
    const { characters: _characters, ...root } = database
    return root
}

export function captureSelectedPersistentCharacter(
    database: Database,
    selectedIndex: number,
): CompleteCharacter | null {
    return database.characters[selectedIndex] ?? null
}

export interface PersistentDataRuntimeStateAdapter {
    captureRoot(): RootDatabase
    captureSelectedCharacter(): CompleteCharacter | null
    getSelectedCharacterId(): string | null | undefined
    replaceDatabase(database: Database): void
    publishCharacter(character: CompleteCharacter): void
    publishConversation(characterId: string, conversation: Chat): void
}

export interface OfficialDatabaseStorage {
    setItem(key: string, value: Uint8Array): Promise<unknown>
}

export interface PersistentDataRuntimeDependencies {
    store: PersistentDataStore
    state: PersistentDataRuntimeStateAdapter
    officialStorage?: OfficialDatabaseStorage | null
    getOfficialStorage?(): OfficialDatabaseStorage | null
    clock?: SaveCoordinatorClock
    onLocalRevision?(revision: DataRevision): void
    onFlushPromise?(promise: Promise<void> | null): void
    onBackgroundError?(error: unknown): void
    prepareDatabase(database: Database): Promise<Database>
}

export interface PersistentDataRuntime {
    readonly store: PersistentDataStore
    readonly revision: DataRevision
    initializeActiveWorkingSet(database: Database): Promise<void>
    markPersistentDataDirty(estimatedBytes: number): void
    flushPendingData(reason: string): Promise<void>
    activateCharacter(id: string): Promise<boolean>
    activateConversation(id: string): Promise<boolean>
    replacePersistentDatabase(database: Database, reason: string): Promise<void>
}

async function concatenateSnapshot(
    store: PersistentDataStore,
    revision: DataRevision,
): Promise<Uint8Array> {
    const chunks: Uint8Array[] = []
    let length = 0
    for await (const chunk of streamRisuSaveFromStore(store, revision)) {
        chunks.push(chunk)
        length += chunk.byteLength
    }
    const bytes = new Uint8Array(length)
    let offset = 0
    for (const chunk of chunks) {
        bytes.set(chunk, offset)
        offset += chunk.byteLength
    }
    return bytes
}

function createOfficialPublisher(
    store: PersistentDataStore,
    getStorage: () => OfficialDatabaseStorage | null,
): OfficialRevisionPublisher {
    return {
        async pin(revision) {
            const storage = getStorage()
            if (!storage) {
                return {
                    publish: async () => undefined,
                    dispose: async () => undefined,
                }
            }
            let bytes: Uint8Array | null = await concatenateSnapshot(store, revision)
            let disposed = false
            return {
                async publish() {
                    if (disposed || !bytes) throw new Error('Pinned publication was disposed')
                    await storage.setItem('database/database.bin', bytes)
                },
                async dispose() {
                    if (disposed) return
                    disposed = true
                    bytes = null
                },
            }
        },
    }
}

export function createPersistentDataRuntime(
    dependencies: PersistentDataRuntimeDependencies,
): PersistentDataRuntime {
    const coordinator = new SaveCoordinator({
        store: dependencies.store,
        captureRoot: dependencies.state.captureRoot,
        captureSelectedCharacter: dependencies.state.captureSelectedCharacter,
        replaceDatabase: dependencies.state.replaceDatabase,
        officialPublisher: dependencies.officialStorage || dependencies.getOfficialStorage
            ? createOfficialPublisher(
                dependencies.store,
                dependencies.getOfficialStorage ?? (() => dependencies.officialStorage ?? null),
            )
            : undefined,
        clock: dependencies.clock,
        onLocalRevision: dependencies.onLocalRevision,
        onFlushPromise: dependencies.onFlushPromise,
        onBackgroundError: dependencies.onBackgroundError,
    })
    const workingSet = new ActiveWorkingSet({
        store: dependencies.store,
        coordinator,
        getSelectedCharacterId: dependencies.state.getSelectedCharacterId,
        publishCharacter: dependencies.state.publishCharacter,
        publishConversation: dependencies.state.publishConversation,
    })
    return {
        store: dependencies.store,
        get revision() {
            return coordinator.revision
        },
        initializeActiveWorkingSet: (database) => workingSet.initializeActiveWorkingSet(database),
        markPersistentDataDirty: (estimatedBytes) =>
            coordinator.markPersistentDataDirty(estimatedBytes),
        flushPendingData: (reason) => coordinator.flushPendingData(reason),
        activateCharacter: (id) => workingSet.activateCharacter(id),
        activateConversation: (id) => workingSet.activateConversation(id),
        async replacePersistentDatabase(database, reason) {
            const prepared = await dependencies.prepareDatabase(database)
            await coordinator.replacePersistentDatabase(prepared, reason)
        },
    }
}
