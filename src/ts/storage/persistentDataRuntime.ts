import type { Chat, Database, character, groupChat } from './database.svelte'
import { ActiveWorkingSet, type CharacterActivationOptions } from './activeWorkingSet.svelte'
import type { DataRevision, PersistentDataStore } from './persistentDataStore'
import {
    SaveCoordinator,
    type CharacterAdditionRequest,
    type OfficialRevisionPublisher,
    type SaveCoordinatorClock,
} from './saveCoordinator'

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
    captureCharacter(id: string): CompleteCharacter | null
    getSelectedCharacterId(): string | null | undefined
    replaceDatabase(database: Database): void
    publishCharacter(character: CompleteCharacter): void
    publishConversation(characterId: string, conversation: Chat): void
}

export interface PersistentDataRuntimeDependencies {
    store: PersistentDataStore
    state: PersistentDataRuntimeStateAdapter
    officialPublisher?: OfficialRevisionPublisher | null
    getOfficialPublisher?(): OfficialRevisionPublisher | null
    clock?: SaveCoordinatorClock
    runExclusiveMigration?<T>(operation: () => Promise<T>): Promise<T>
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
    commitCharacterAddition(request: CharacterAdditionRequest, reason: string): Promise<void>
    activateCharacter(id: string, options?: CharacterActivationOptions): Promise<boolean>
    activateConversation(id: string): Promise<boolean>
    replacePersistentDatabase(database: Database, reason: string): Promise<void>
    runMigration<T>(reason: string, operation: () => Promise<T>): Promise<T>
    adoptActivatedDatabase(database: Database, revision: DataRevision): Promise<void>
    publishCurrentOfficialRevision(): Promise<void>
}

function createDynamicOfficialPublisher(
    getPublisher: () => OfficialRevisionPublisher | null,
): OfficialRevisionPublisher {
    return {
        async pin(revision) {
            const publisher = getPublisher()
            if (!publisher) {
                return {
                    publish: async () => undefined,
                    dispose: async () => undefined,
                }
            }
            return publisher.pin(revision)
        },
    }
}

export function createPersistentDataRuntime(
    dependencies: PersistentDataRuntimeDependencies,
): PersistentDataRuntime {
    let workingSet: ActiveWorkingSet
    const coordinator = new SaveCoordinator({
        store: dependencies.store,
        captureRoot: dependencies.state.captureRoot,
        captureSelectedCharacter: dependencies.state.captureSelectedCharacter,
        captureCharacter: dependencies.state.captureCharacter,
        replaceDatabase: dependencies.state.replaceDatabase,
        officialPublisher: dependencies.officialPublisher || dependencies.getOfficialPublisher
            ? createDynamicOfficialPublisher(
                dependencies.getOfficialPublisher
                    ?? (() => dependencies.officialPublisher ?? null),
            )
            : undefined,
        clock: dependencies.clock,
        runExclusiveMigration: dependencies.runExclusiveMigration,
        invalidateNavigation: () => workingSet.invalidateNavigation(),
        onLocalRevision: dependencies.onLocalRevision,
        onFlushPromise: dependencies.onFlushPromise,
        onBackgroundError: dependencies.onBackgroundError,
    })
    workingSet = new ActiveWorkingSet({
        store: dependencies.store,
        coordinator,
        getSelectedCharacterId: dependencies.state.getSelectedCharacterId,
        publishCharacter: dependencies.state.publishCharacter,
        publishConversation: dependencies.state.publishConversation,
    })
    const activateCharacter = (
        id: string,
        options?: CharacterActivationOptions,
    ): Promise<boolean> => {
        const prepare = options?.prepare
        return workingSet.activateCharacter(id, prepare ? {
            async prepare() {
                const prepared = await prepare()
                if (!prepared) return null
                return {
                    ...prepared,
                    database: await dependencies.prepareDatabase(prepared.database),
                }
            },
        } : undefined)
    }
    return {
        store: dependencies.store,
        get revision() {
            return coordinator.revision
        },
        initializeActiveWorkingSet: (database) => workingSet.initializeActiveWorkingSet(database),
        markPersistentDataDirty: (estimatedBytes) =>
            coordinator.markPersistentDataDirty(estimatedBytes),
        flushPendingData: (reason) => coordinator.flushPendingData(reason),
        commitCharacterAddition: (request, reason) =>
            coordinator.commitCharacterAddition(request, reason),
        activateCharacter,
        activateConversation: (id) => workingSet.activateConversation(id),
        async replacePersistentDatabase(database, reason) {
            const prepared = await dependencies.prepareDatabase(database)
            await coordinator.replacePersistentDatabase(prepared, reason)
        },
        runMigration: (reason, operation) => coordinator.runMigration(reason, operation),
        adoptActivatedDatabase: (database, revision) =>
            coordinator.adoptActivatedDatabase(database, revision),
        publishCurrentOfficialRevision: () => coordinator.publishCurrentOfficialRevision(),
    }
}
