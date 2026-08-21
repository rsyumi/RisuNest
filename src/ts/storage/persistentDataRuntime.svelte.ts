import { get } from 'svelte/store'
import { ReloadGUIPointer, selectedCharID } from '../stores.svelte'
import type { Chat, Database, character, groupChat } from './database.svelte'
import { getDatabase, setDatabase } from './database.svelte'
import { prepareDatabaseForPersistence } from './databasePreparation'
import { getPersistentDataStore } from './persistentDataStoreFactory'
import type { DataRevision } from './persistentDataStore'
import {
    createPersistentDataRuntime,
    type OfficialDatabaseStorage,
    type PersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'

export type {
    OfficialDatabaseStorage,
    PersistentDataRuntime,
    PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
export { createPersistentDataRuntime } from './persistentDataRuntime'

type CompleteCharacter = character | groupChat

function productionStateAdapter(): PersistentDataRuntimeStateAdapter {
    return {
        captureRoot() {
            const { characters: _characters, ...root } = getDatabase({ snapshot: true })
            return root
        },
        captureSelectedCharacter(): CompleteCharacter | null {
            const database = getDatabase({ snapshot: true })
            return database.characters[get(selectedCharID)] ?? null
        },
        getSelectedCharacterId() {
            return getDatabase().characters[get(selectedCharID)]?.chaId
        },
        replaceDatabase(database) {
            setDatabase(database)
        },
        publishCharacter(character) {
            const database = getDatabase()
            const index = database.characters.findIndex((candidate) => candidate.chaId === character.chaId)
            if (index < 0) return
            database.characters[index] = character
            selectedCharID.set(index)
        },
        publishConversation(characterId, conversation: Chat) {
            const database = getDatabase()
            const characterIndex = database.characters.findIndex(
                (candidate) => candidate.chaId === characterId,
            )
            if (characterIndex < 0) return
            const character = database.characters[characterIndex]
            const conversationIndex = character.chats.findIndex(
                (candidate) => candidate.id === conversation.id,
            )
            if (conversationIndex < 0) return
            character.chats[conversationIndex] = conversation
            character.chatPage = conversationIndex
            selectedCharID.set(characterIndex)
            ReloadGUIPointer.set(Math.random())
        },
    }
}

interface ProductionRuntimeConfiguration {
    officialStorage: OfficialDatabaseStorage | null
    onLocalRevision?: (revision: DataRevision) => void
    onFlushPromise?: (promise: Promise<void> | null) => void
    onBackgroundError?: (error: unknown) => void
}

const productionConfiguration: ProductionRuntimeConfiguration = {
    officialStorage: null,
}
let productionRuntime: PersistentDataRuntime | null = null

export function configurePersistentDataRuntime(
    configuration: Partial<ProductionRuntimeConfiguration>,
): void {
    Object.assign(productionConfiguration, configuration)
}

export function getPersistentDataRuntime(): PersistentDataRuntime {
    if (!productionRuntime) {
        productionRuntime = createPersistentDataRuntime({
            store: getPersistentDataStore(),
            state: productionStateAdapter(),
            officialStorage: productionConfiguration.officialStorage,
            onLocalRevision: (revision) => productionConfiguration.onLocalRevision?.(revision),
            onFlushPromise: (promise) => productionConfiguration.onFlushPromise?.(promise),
            onBackgroundError: (error) => productionConfiguration.onBackgroundError?.(error),
            prepareDatabase: prepareDatabaseForPersistence,
        })
    }
    return productionRuntime
}

export const initializeActiveWorkingSet = (database: Database): Promise<void> =>
    getPersistentDataRuntime().initializeActiveWorkingSet(database)
export const markPersistentDataDirty = (estimatedBytes: number): void =>
    getPersistentDataRuntime().markPersistentDataDirty(estimatedBytes)
export const flushPendingData = (reason: string): Promise<void> =>
    getPersistentDataRuntime().flushPendingData(reason)
export const activateCharacter = (id: string): Promise<boolean> =>
    getPersistentDataRuntime().activateCharacter(id)
export const activateConversation = (id: string): Promise<boolean> =>
    getPersistentDataRuntime().activateConversation(id)
export const replacePersistentDatabase = (database: Database, reason: string): Promise<void> =>
    getPersistentDataRuntime().replacePersistentDatabase(database, reason)
