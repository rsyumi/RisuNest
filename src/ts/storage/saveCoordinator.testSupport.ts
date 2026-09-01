import { vi } from 'vitest'
import {
    SaveCoordinator as ProductionSaveCoordinator,
    type SaveCoordinatorDependencies,
} from './saveCoordinator'
import type { Database } from './database.svelte'
import type { PersistentDataStore } from './persistentDataStore'

export function makeDatabase(): Database {
    return {
        username: 'Fixture',
        botPresets: [],
        characters: [
            {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chats: [],
            },
        ],
    } as unknown as Database
}

export function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (error: unknown) => void
    const promise = new Promise<T>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, resolve, reject }
}

export function makeStore(commit = vi.fn()) {
    return {
        commit,
        replaceFromDatabase: vi.fn(),
    } as unknown as PersistentDataStore
}

export function captureRoot(
    database: Database,
): Omit<Database, 'characters' | 'botPresets' | 'pluginCustomStorage'> {
    const {
        characters: _characters,
        botPresets: _botPresets,
        pluginCustomStorage: _pluginCustomStorage,
        ...root
    } = database
    return root
}

type TestCoordinatorDependencies = Omit<SaveCoordinatorDependencies, 'captureCharacter'> &
    Partial<Pick<SaveCoordinatorDependencies, 'captureCharacter'>>

export class SaveCoordinator extends ProductionSaveCoordinator {
    constructor(dependencies: TestCoordinatorDependencies) {
        super({
            ...dependencies,
            captureCharacter: dependencies.captureCharacter ?? ((id) => {
                const selected = dependencies.captureSelectedCharacter()
                return selected?.chaId === id ? selected : null
            }),
        })
    }
}
