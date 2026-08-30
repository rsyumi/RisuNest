import { describe, expect, it, vi } from 'vitest'
import {
    PersistentRootModuleAppendRejectedError,
    SaveCoordinator as ProductionSaveCoordinator,
    canonicalJson,
    type SaveCoordinatorDependencies,
    type WindowedConversationPersistenceAuthority,
} from './saveCoordinator'
import type { Chat, Database, character, groupChat } from './database.svelte'
import type { PersistentDataStore, WorkingSetCommit } from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'
import { createPluginStorageStore } from '../plugins/pluginStorageStore'
import { createConversationSummaryStubFromChat } from './conversationResidency'
import {
    ActiveConversationSession,
    cloneConversationMetadata,
} from './activeConversationSession'

function makeDatabase(): Database {
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

describe('canonical JSON property safety', () => {
    it('preserves JSON-origin own proto keys at every nested level', () => {
        const source = JSON.parse(
            '{"zeta":0,"__proto__":{"nested":{"__proto__":false}},"alpha":""}',
        )

        const canonical = JSON.parse(canonicalJson(source)) as Record<string, unknown>
        const protoValue = canonical.__proto__ as Record<string, unknown>
        const nested = protoValue.nested as Record<string, unknown>

        expect(Object.keys(canonical)).toEqual(['__proto__', 'alpha', 'zeta'])
        expect(Object.hasOwn(canonical, '__proto__')).toBe(true)
        expect(Object.getPrototypeOf(canonical)).toBe(Object.prototype)
        expect(Object.hasOwn(nested, '__proto__')).toBe(true)
        expect(nested.__proto__).toBe(false)
        expect(Object.getPrototypeOf(nested)).toBe(Object.prototype)
        expect(canonical.alpha).toBe('')
        expect(canonical.zeta).toBe(0)
    })
})

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (error: unknown) => void
    const promise = new Promise<T>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, resolve, reject }
}

function makeStore(commit = vi.fn()) {
    return {
        commit,
        replaceFromDatabase: vi.fn(),
    } as unknown as PersistentDataStore
}

function captureRoot(
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

function makeGroupDeletionLease(database: Database, revision = 1) {
    const authoritative = structuredClone(database)
    return {
        revision,
        readRoot: vi.fn(async () => ({ revision, value: captureRoot(authoritative) })),
        queryCharacters: vi.fn(async ({ trash }: { trash: boolean }) => ({
            revision,
            items: trash
                ? []
                : authoritative.characters.map((item, configuredIndex) => ({
                    id: item.chaId,
                    name: item.name,
                    configuredIndex,
                    recentAt: 0,
                    trashed: false,
                    conversationCount: item.chats.length,
                    type: item.type,
                })),
        })),
        readCharacter: vi.fn(async (id: string) => {
            const character = authoritative.characters.find((item) => item.chaId === id)
            if (!character) return null
            const { chats: _chats, ...detail } = character
            return { revision, value: detail }
        }),
        release: vi.fn(async () => undefined),
    }
}

function publishGroupDeletion(database: Database, state: any): void {
    Object.assign(database, state.root)
    for (const detail of state.relatedCharacters ?? []) {
        const live = database.characters.find((item) => item.chaId === detail.chaId)
        if (live) Object.assign(live, detail)
    }
    database.characters = database.characters.filter(
        (item) => item.chaId !== state.characterId,
    )
}

type TestCoordinatorDependencies = Omit<SaveCoordinatorDependencies, 'captureCharacter'> &
    Partial<Pick<SaveCoordinatorDependencies, 'captureCharacter'>>

class SaveCoordinator extends ProductionSaveCoordinator {
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

describe('SaveCoordinator', () => {
    function makeAdditionDatabase() {
        const database = makeDatabase()
        const added = structuredClone(database.characters[0])
        added.chaId = 'char-added'
        added.name = 'Added'
        return { database, added }
    }

    it('atomically appends a root module while carrying every occurrence owner head', async () => {
        const database = makeDatabase()
        database.modules = [
            { id: 'duplicate', name: 'First', description: '', assets: [] },
            { id: 'duplicate', name: 'Second', description: '' },
        ]
        database.personas = [
            { name: 'With module', embeddedModule: { id: 'embedded', name: 'Embedded', assets: [] } },
            { name: 'Without module' },
        ] as Database['personas']
        const existingHeads = new Map([
            ['root-module-assets:0', {
                owner: { kind: 'root-module-assets', index: 0 },
                present: true,
                manifestHash: '1'.repeat(64),
                entryCount: 0,
            }],
            ['root-module-assets:1', {
                owner: { kind: 'root-module-assets', index: 1 },
                present: false,
                manifestHash: null,
                entryCount: 0,
            }],
            ['persona-embedded-module-assets:0', {
                owner: { kind: 'persona-embedded-module-assets', index: 0 },
                present: true,
                manifestHash: '2'.repeat(64),
                entryCount: 0,
            }],
        ])
        const readAssetOwnerHead = vi.fn(async (owner: { kind: string; index?: number }) => {
            const head = existingHeads.get(`${owner.kind}:${owner.index}`)
            return head ? { revision: 7, value: structuredClone(head) } : null
        })
        const release = vi.fn(async () => undefined)
        const commit = vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 }))
        const publishRootWorkingSet = vi.fn((root) => Object.assign(database, root))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead,
                readAssetAliasesByKeys: vi.fn(async () => ({ revision: 7, value: [] })),
                release,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet,
        })
        coordinator.initialize(7, database)
        const alias = {
            kind: 'asset' as const,
            key: `assets/${'a'.repeat(64)}.PNG`,
            objectHash: 'a'.repeat(64),
            size: 4,
            mime: '',
            name: '',
            ext: 'PNG',
        }

        await coordinator.appendPersistentRootModule('native-risum-import', {
            module: {
                id: 'new-id',
                name: 'Imported',
                description: '',
                assets: [['same', alias.key, 'PNG']],
            },
            assetAliases: [alias],
            ownerHead: {
                present: true,
                manifestHash: '3'.repeat(64),
                entryCount: 1,
            },
        })

        expect(readAssetOwnerHead.mock.calls.map(([owner]) => owner)).toEqual([
            { kind: 'root-module-assets', index: 0 },
            { kind: 'root-module-assets', index: 1 },
            { kind: 'persona-embedded-module-assets', index: 0 },
        ])
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            root: expect.objectContaining({
                modules: [
                    { id: 'duplicate', name: 'First', description: '', assets: [] },
                    { id: 'duplicate', name: 'Second', description: '' },
                    {
                        id: 'new-id',
                        name: 'Imported',
                        description: '',
                        assets: [['same', alias.key, 'PNG']],
                    },
                ],
            }),
            assetAliases: [alias],
            assetOwnerHeads: [
                existingHeads.get('root-module-assets:0'),
                existingHeads.get('root-module-assets:1'),
                existingHeads.get('persona-embedded-module-assets:0'),
                {
                    owner: { kind: 'root-module-assets', index: 2 },
                    present: true,
                    manifestHash: '3'.repeat(64),
                    entryCount: 1,
                },
            ],
        })
        expect(release).toHaveBeenCalledOnce()
        expect(publishRootWorkingSet).toHaveBeenCalledOnce()
        expect(database.modules.at(-1)).toEqual({
            id: 'new-id',
            name: 'Imported',
            description: '',
            assets: [['same', alias.key, 'PNG']],
        })
        expect(coordinator.revision).toBe(8)
    })

    it('retains a concurrent live root mutation while the module commit awaits', async () => {
        const database = makeDatabase()
        database.modules = []
        let resolveCommit!: (value: { revision: number }) => void
        const commit = vi.fn(() => new Promise<{ revision: number }>((resolve) => {
            resolveCommit = resolve
        }))
        const publishRootWorkingSet = vi.fn((root) => Object.assign(database, root))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet,
        })
        coordinator.initialize(7, database)

        const append = coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.username = 'Concurrent username'
        resolveCommit({ revision: 8 })
        await append

        expect(database.username).toBe('Concurrent username')
        expect(database.modules.at(-1)?.id).toBe('new-id')
    })

    it('returns local module commit success before deferred official publication', async () => {
        const database = makeDatabase()
        database.modules = []
        const scheduled: Array<() => void> = []
        const clock = {
            setTimeout: (callback: () => void) => {
                scheduled.push(callback)
                return callback
            },
            clearTimeout: vi.fn(),
        }
        const pin = vi.fn(async () => { throw new Error('offline') })
        const store = {
            commit: vi.fn(async () => ({ revision: 8 })),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet: (root) => Object.assign(database, root),
            officialPublisher: { pin },
            clock,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).resolves.toBeUndefined()

        expect(coordinator.revision).toBe(8)
        expect(database.modules.at(-1)?.id).toBe('new-id')
        expect(pin).not.toHaveBeenCalled()
        expect(scheduled).toHaveLength(1)
    })

    it.each([
        {
            name: 'revision acquisition',
            acquireRevision: async () => { throw new Error('lease unavailable') },
        },
        {
            name: 'root read',
            acquireRevision: async () => ({
                revision: 7,
                readRoot: vi.fn(async () => { throw new Error('root unavailable') }),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            }),
        },
        {
            name: 'revision release',
            acquireRevision: async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(makeDatabase()) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => { throw new Error('release unavailable') }),
            }),
        },
    ])('classifies $name failure before commit as a known rejected module append', async ({
        acquireRevision,
    }) => {
        const database = makeDatabase()
        database.modules = []
        const commit = vi.fn()
        const coordinator = new SaveCoordinator({
            store: { commit, acquireRevision } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toBeInstanceOf(PersistentRootModuleAppendRejectedError)

        expect(commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('classifies a synchronous precondition failure before queueing as a rejected module append', () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })

        expect(() => coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).toThrow(PersistentRootModuleAppendRejectedError)
    })

    it('classifies a revision conflict from commit as a rejected module append', async () => {
        const database = makeDatabase()
        database.modules = []
        const commitError = new RevisionConflictError(7, 8)
        const store = {
            commit: vi.fn(async () => { throw commitError }),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toMatchObject({
            name: 'PersistentRootModuleAppendRejectedError',
            message: commitError.message,
        })

        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('retains a post-commit callback revision conflict as the exact ambiguous error', async () => {
        const database = makeDatabase()
        database.modules = []
        const callbackError = new RevisionConflictError(7, 8)
        const store = {
            commit: vi.fn(async () => ({ revision: 8 })),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet: () => { throw callbackError },
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toBe(callbackError)

        expect(store.commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(8)
    })

    it('retains the exact ambiguous error when commit invocation rejects generically', async () => {
        const database = makeDatabase()
        database.modules = []
        const commitError = new Error('commit response lost')
        const store = {
            commit: vi.fn(async () => { throw commitError }),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toBe(commitError)

        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('preserves cancellation during an alias read and never starts commit', async () => {
        const database = makeDatabase()
        database.modules = []
        const aliasRead = deferred<{ revision: number; value: [] }>()
        const readAssetAliasesByKeys = vi.fn(() => aliasRead.promise)
        const release = vi.fn(async () => undefined)
        const commit = vi.fn()
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAliasesByKeys,
                release,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)
        const controller = new AbortController()
        const reason = new DOMException('cancelled during alias read', 'AbortError')

        const append = coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [{
                kind: 'asset',
                key: `assets/${'a'.repeat(64)}.bin`,
                objectHash: 'a'.repeat(64),
                size: 4,
                mime: '',
                name: '',
                ext: 'bin',
            }],
            ownerHead: { present: true, manifestHash: 'b'.repeat(64), entryCount: 1 },
        }, controller.signal)
        await vi.waitFor(() => expect(readAssetAliasesByKeys).toHaveBeenCalledOnce())
        controller.abort(reason)
        aliasRead.resolve({ revision: 7, value: [] })

        await expect(append).rejects.toBe(reason)
        expect(commit).not.toHaveBeenCalled()
        expect(release).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('preserves an existing dirty-save debounce when cancellation is queued', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            database.modules = []
            const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => ({
                revision: expectedRevision + 1,
            }))
            const store = makeStore(commit)
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(7, database)
            database.username = 'Pending dirty edit'
            coordinator.markPersistentDataDirty(1)
            const controller = new AbortController()
            const reason = new RevisionConflictError(7, 8)
            controller.abort(reason)

            await expect(coordinator.appendPersistentRootModule('native-risum-import', {
                module: { id: 'new-id', name: 'Imported', description: '' },
                assetAliases: [],
                ownerHead: { present: false, manifestHash: null, entryCount: 0 },
            }, controller.signal)).rejects.toBe(reason)

            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(expect.objectContaining({
                expectedRevision: 7,
                root: { username: 'Pending dirty edit', modules: [] },
            }))
        } finally {
            vi.useRealTimers()
        }
    })

    it('omits matching aliases to retain metadata and rejects conflicting identities', async () => {
        const database = makeDatabase()
        database.modules = []
        const existing = {
            kind: 'asset' as const,
            key: `assets/${'a'.repeat(64)}.bin`,
            objectHash: 'a'.repeat(64),
            size: 4,
            mime: 'application/octet-stream',
            name: 'retained.bin',
            ext: 'future-extension-metadata',
        }
        const commit = vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 }))
        const readAssetAliasesByKeys = vi.fn(async () => ({ revision: 7, value: [existing] }))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAliasesByKeys,
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)
        const incoming = { ...existing, mime: '', name: '', ext: '.unsafe/path' }

        await coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [incoming, incoming],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })

        expect(readAssetAliasesByKeys).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].assetAliases).toEqual([])

        const conflictingReadAssetAliasesByKeys = vi.fn(async () => ({
            revision: 7,
            value: [{ ...existing, objectHash: 'b'.repeat(64) }],
        }))
        const conflictingCoordinator = new SaveCoordinator({
            store: {
                ...store,
                acquireRevision: vi.fn(async () => ({
                    revision: 7,
                    readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                    readAssetOwnerHead: vi.fn(async () => null),
                    readAssetAliasesByKeys: conflictingReadAssetAliasesByKeys,
                    release: vi.fn(async () => undefined),
                })),
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        conflictingCoordinator.initialize(7, database)

        await expect(conflictingCoordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'conflict', name: 'Conflict', description: '' },
            assetAliases: [
                incoming,
                ...Array.from({ length: 512 }, (_, index) => ({
                    ...incoming,
                    key: `assets/conflict-${index}.bin`,
                    objectHash: index.toString(16).padStart(64, '0'),
                })),
            ],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toThrow(/alias conflicts/i)
        expect(conflictingReadAssetAliasesByKeys).toHaveBeenCalledOnce()
    })

    it('looks up 513 unique aliases in stable batches and commits only missing aliases', async () => {
        const database = makeDatabase()
        database.modules = []
        const aliases = Array.from({ length: 513 }, (_, index) => ({
            kind: 'asset' as const,
            key: `assets/${index.toString(16).padStart(64, '0')}.bin`,
            objectHash: index.toString(16).padStart(64, '0'),
            size: index,
            mime: '',
            name: '',
            ext: 'bin',
        }))
        const existing = {
            ...aliases[0],
            mime: 'application/octet-stream',
            name: 'retained.bin',
            ext: 'future-extension-metadata',
        }
        const commit = vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 }))
        const readAssetAliasesByKeys = vi.fn(async (_kind: 'asset', keys: string[]) => ({
            revision: 7,
            value: keys.includes(existing.key) ? [existing] : [],
        }))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAliasesByKeys,
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'batched', name: 'Batched', description: '' },
            assetAliases: [...aliases, aliases[0]],
            ownerHead: { present: true, manifestHash: 'f'.repeat(64), entryCount: 514 },
        })

        expect(readAssetAliasesByKeys.mock.calls.map(([, keys]) => keys)).toEqual([
            aliases.slice(0, 512).map((alias) => alias.key),
            [aliases[512].key],
        ])
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].assetAliases).toEqual(aliases.slice(1))
    })

    it('adopts a successful storage-only revision without changing dirty state or baselines', async () => {
        const database = makeDatabase()
        const commit = vi.fn()
        const onStorageOnlyRevision = vi.fn()
        const onLocalRevision = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onStorageOnlyRevision,
            onLocalRevision,
        })
        coordinator.initialize(4, database)
        const generation = coordinator.mutationGeneration
        const authorityEpoch = coordinator.storageAuthorityEpoch

        await coordinator.runStorageOnlyMutation(async (expectedRevision) => {
            expect(expectedRevision).toBe(4)
            return 5
        })

        expect(coordinator.revision).toBe(5)
        expect(coordinator.mutationGeneration).toBe(generation)
        expect(coordinator.storageAuthorityEpoch).toBe(authorityEpoch)
        expect(coordinator.pendingBytes).toBe(0)
        expect(onStorageOnlyRevision).toHaveBeenCalledWith(5)
        expect(onLocalRevision).toHaveBeenCalledWith(5)
        await coordinator.flushPendingData('storage-only-baseline')
        expect(commit).not.toHaveBeenCalled()
    })

    it('does not advance storage-only state when the mutation fails', async () => {
        const database = makeDatabase()
        const error = new Error('cold mutation failed')
        const onStorageOnlyRevision = vi.fn()
        const onLocalRevision = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onStorageOnlyRevision,
            onLocalRevision,
        })
        coordinator.initialize(4, database)
        const generation = coordinator.mutationGeneration

        await expect(coordinator.runStorageOnlyMutation(async () => {
            throw error
        })).rejects.toBe(error)

        expect(coordinator.revision).toBe(4)
        expect(coordinator.mutationGeneration).toBe(generation)
        expect(coordinator.pendingBytes).toBe(0)
        expect(onStorageOnlyRevision).not.toHaveBeenCalled()
        expect(onLocalRevision).not.toHaveBeenCalled()
    })

    it('serializes storage-only mutation after an ordinary save without losing either revision', async () => {
        const database = makeDatabase()
        const committed = deferred<{ revision: number }>()
        const commit = vi.fn(() => committed.promise)
        const storageMutation = vi.fn(async (expectedRevision: number) => expectedRevision + 1)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4, database)
        database.username = 'Ordinary save'
        coordinator.markPersistentDataDirty(1)

        const flushing = coordinator.flushPendingData('ordinary-before-cold')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        const storing = coordinator.runStorageOnlyMutation(storageMutation)
        expect(storageMutation).not.toHaveBeenCalled()

        committed.resolve({ revision: 5 })
        await flushing
        await storing

        expect(commit).toHaveBeenCalledWith(expect.objectContaining({ expectedRevision: 4 }))
        expect(storageMutation).toHaveBeenCalledWith(5)
        expect(coordinator.revision).toBe(6)
    })

    it('commits captured plugin storage mutations atomically without putting values in root', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { alpha: 'old', removed: true }
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)
        database.pluginCustomStorage.alpha = 'new'
        database.pluginCustomStorage.beta = { nested: true }
        delete database.pluginCustomStorage.removed
        database.username = 'Root changed too'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('plugin-storage')

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            root: { username: 'Root changed too' },
            pluginStorage: [
                { type: 'delete', key: 'removed' },
                { type: 'set', key: 'alpha', value: 'new' },
                { type: 'set', key: 'beta', value: { nested: true } },
            ],
        })
        expect(commit.mock.calls[0][0].root).not.toHaveProperty('pluginCustomStorage')
    })

    it('does not clear plugin storage when the scalable working set omits it', async () => {
        const database = makeDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)
        database.username = 'Scalable edit'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('scalable-plugin-storage')

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            root: { username: 'Scalable edit' },
        })
    })

    it('serializes explicit plugin mutations through revision CAS', async () => {
        const database = makeDatabase()
        const committed = deferred<{ revision: number }>()
        const commit = vi.fn(() => committed.promise)
        const onLocalRevision = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onLocalRevision,
        })
        coordinator.initialize(7)

        const mutation = coordinator.mutatePersistentPluginStorage('v3-plugin-storage', [
            { type: 'set', key: 'alpha', value: { large: true } },
        ])
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            pluginStorage: [
                { type: 'set', key: 'alpha', value: { large: true } },
            ],
        })
        expect(coordinator.revision).toBe(7)

        committed.resolve({ revision: 8 })
        await mutation

        expect(coordinator.revision).toBe(8)
        expect(onLocalRevision).toHaveBeenCalledWith(8)
    })

    it('does not hydrate plugin values into an incomplete scalable working set', async () => {
        const database = makeDatabase()
        const publishPluginStorageWorkingSet = vi.fn()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            isIncompleteWorkingSet: () => true,
            publishPluginStorageWorkingSet,
        })
        coordinator.initialize(7, database)

        await coordinator.mutatePersistentPluginStorage('scalable-v3', [
            { type: 'set', key: 'large', value: 'external-only' },
        ])

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            pluginStorage: [{ type: 'set', key: 'large', value: 'external-only' }],
        })
        expect(publishPluginStorageWorkingSet).not.toHaveBeenCalled()
        expect(database).not.toHaveProperty('pluginCustomStorage')
    })

    it('publishes explicit V3 mutations into a hydrated compatibility working set', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { existing: true }
        const storagePrototype = Object.getPrototypeOf(database.pluginCustomStorage)
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPluginStorageWorkingSet: (storage) => {
                database.pluginCustomStorage = storage
            },
        })
        coordinator.initialize(2, database)

        await coordinator.mutatePersistentPluginStorage('maximum-compatibility', [
            { type: 'set', key: 'added', value: 42 },
            { type: 'set', key: '__proto__', value: 0 },
        ])

        expect(Object.keys(database.pluginCustomStorage)).toEqual([
            'existing',
            'added',
            '__proto__',
        ])
        expect(Object.hasOwn(database.pluginCustomStorage, '__proto__')).toBe(true)
        expect(database.pluginCustomStorage.__proto__).toBe(0)
        expect(Object.getPrototypeOf(database.pluginCustomStorage)).toBe(storagePrototype)
        await coordinator.flushPendingData('already-baselined')
        expect(commit).toHaveBeenCalledOnce()
    })

    it('rebases a later same-key V2 mutation over an in-flight V3 commit', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { shared: 'base' }
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPluginStorageWorkingSet: (storage) => {
                database.pluginCustomStorage = storage
            },
        })
        coordinator.initialize(3, database)

        const v3Mutation = coordinator.mutatePersistentPluginStorage('v3-race', [
            { type: 'set', key: 'shared', value: 'v3-first' },
        ])
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.pluginCustomStorage.shared = 'v2-later'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 4 })
        await v3Mutation

        expect(database.pluginCustomStorage.shared).toBe('v2-later')
        await coordinator.flushPendingData('persist-v2-winner')
        expect(commit).toHaveBeenLastCalledWith({
            expectedRevision: 4,
            pluginStorage: [{ type: 'set', key: 'shared', value: 'v2-later' }],
        })
    })

    it('keeps V3 reads coherent with a later same-key V2 mutation after commit publication', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { shared: 'base' }
        const durableStorage: Record<string, unknown> = { shared: 'base' }
        let revision = 3
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn(async (input) => {
            const result = commit.mock.calls.length === 1
                ? await firstCommit.promise
                : { revision: input.expectedRevision + 1 }
            for (const mutation of input.pluginStorage ?? []) {
                if (mutation.type === 'clear') {
                    for (const key of Object.keys(durableStorage)) delete durableStorage[key]
                } else if (mutation.type === 'delete') {
                    delete durableStorage[mutation.key]
                } else {
                    durableStorage[mutation.key] = structuredClone(mutation.value)
                }
            }
            revision = result.revision
            return result
        })
        const store = {
            ...makeStore(commit),
            open: vi.fn(async () => undefined),
            queryPluginStorage: vi.fn(async () => ({
                revision,
                items: Object.keys(durableStorage).map((key) => ({ key, byteSize: 1 })),
            })),
            readPluginStorage: vi.fn(async (key: string) =>
                Object.prototype.hasOwnProperty.call(durableStorage, key)
                    ? { revision, value: structuredClone(durableStorage[key]) }
                    : null),
        } as unknown as PersistentDataStore
        let v3Storage: ReturnType<typeof createPluginStorageStore>
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPluginStorageWorkingSet: (storage) => {
                database.pluginCustomStorage = storage
                v3Storage?.synchronizeCompatibilityStorage(storage)
            },
        })
        coordinator.initialize(3, database)
        v3Storage = createPluginStorageStore({
            store,
            mutate: (mutations) => coordinator.mutatePersistentPluginStorage(
                'overlapping-v3-v2',
                mutations,
            ),
        }, 100)

        const v3Mutation = v3Storage.setItem('shared', 'v3-first')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.pluginCustomStorage.shared = 'v2-later'
        v3Storage.synchronizeCompatibilityMutation({
            type: 'set',
            key: 'shared',
            value: 'v2-later',
        })
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 4 })
        await v3Mutation
        await coordinator.flushPendingData('persist-v2-winner')

        expect(durableStorage.shared).toBe('v2-later')
        expect(database.pluginCustomStorage.shared).toBe('v2-later')
        await expect(v3Storage.getItem('shared')).resolves.toBe('v2-later')
    })

    it('commits the complete preset array atomically with a changed root', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)
        database.username = 'Changed with presets'
        database.botPresets = [{ name: 'Resident preset', mainPrompt: 'complete' }] as Database['botPresets']
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('preset-change')

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 2,
            root: expect.objectContaining({ username: 'Changed with presets' }),
            replacePresets: database.botPresets,
        })
        expect(vi.mocked(store.commit).mock.calls[0][0].root).not.toHaveProperty('botPresets')
    })

    it('never replaces persisted presets from a partial scalable working set', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Active only', mainPrompt: 'resident' }] as Database['botPresets']
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(5)
        database.username = 'Root edit with a partial preset working set'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('partial-preset-working-set')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 5,
            root: expect.objectContaining({ username: 'Root edit with a partial preset working set' }),
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replacePresets')
    })

    it('serializes an explicit preset mutation and publishes only after its atomic commit', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Projected active' }] as Database['botPresets']
        const committed = deferred<{ revision: number }>()
        const store = {
            commit: vi.fn(() => committed.promise),
            readRoot: vi.fn(async () => ({
                revision: 3,
                value: captureRoot(database),
            })),
            queryPresets: vi.fn(async () => ({
                revision: 3,
                items: [
                    { id: '0', configuredIndex: 0, name: 'First' },
                    { id: '1', configuredIndex: 1, name: 'Second' },
                ],
            })),
            readPreset: vi.fn(async (id: string) => ({
                revision: 3,
                value: { name: id === '0' ? 'First' : 'Second', mainPrompt: id },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet,
        })
        coordinator.initialize(3)

        const mutation = coordinator.mutatePersistentPresets('rename', ({ root, presets }) => {
            root.botPresetsId = 1
            presets[1].name = 'Renamed'
        })
        await vi.waitFor(() => expect(store.commit).toHaveBeenCalledTimes(1))

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 3,
            root: expect.objectContaining({ botPresetsId: 1 }),
            replacePresets: [
                { name: 'First', mainPrompt: '0' },
                { name: 'Renamed', mainPrompt: '1' },
            ],
        })
        expect(publishPresetWorkingSet).not.toHaveBeenCalled()

        committed.resolve({ revision: 4 })
        await mutation

        expect(publishPresetWorkingSet).toHaveBeenCalledWith({
            revision: 4,
            root: expect.objectContaining({ botPresetsId: 1 }),
            presets: [
                { name: 'First', mainPrompt: '0' },
                { name: 'Renamed', mainPrompt: '1' },
            ],
        })
        expect(coordinator.revision).toBe(4)
    })

    it('does not publish an explicit preset mutation when the atomic commit fails', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn().mockRejectedValue(new Error('preset commit failed')),
            readRoot: vi.fn(async () => ({ revision: 8, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 8,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 8,
                value: { name: 'First', mainPrompt: 'full' },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet,
        })
        coordinator.initialize(8)

        await expect(coordinator.mutatePersistentPresets('rename', ({ presets }) => {
            presets[0].name = 'Never published'
        })).rejects.toThrow('preset commit failed')

        expect(publishPresetWorkingSet).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(8)
    })

    it('preserves and flushes a maximum preset edit made while explicit commit is pending', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Initial', mainPrompt: 'initial' }] as Database['botPresets']
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 20, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 20,
                items: [{ id: '0', configuredIndex: 0, name: 'Initial' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 20,
                value: { name: 'Initial', mainPrompt: 'initial' },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet,
        })
        coordinator.initialize(20)

        const mutation = coordinator.mutatePersistentPresets('explicit-rename', ({ presets }) => {
            presets[0].name = 'Explicit'
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.botPresets[0].name = 'Concurrent live edit'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 21 })

        await expect(mutation).rejects.toThrow('changed during preset mutation')

        expect(publishPresetWorkingSet).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toEqual({
            expectedRevision: 21,
            replacePresets: [{ name: 'Concurrent live edit', mainPrompt: 'initial' }],
        })
        expect(database.botPresets[0].name).toBe('Concurrent live edit')
        expect(coordinator.revision).toBe(22)
    })

    it('rejects an incomplete live replacement before cloning or capturing state', async () => {
        const database = makeDatabase()
        const candidate = makeDatabase() as Database & { cloneTrap?: unknown }
        Object.defineProperty(candidate, 'cloneTrap', {
            enumerable: true,
            get() {
                throw new Error('candidate was cloned')
            },
        })
        const captureRootValue = vi.fn(() => captureRoot(database))
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: captureRootValue,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            isIncompleteWorkingSet: (value) => value === candidate,
        })
        coordinator.initialize(1)
        captureRootValue.mockClear()

        await expect(coordinator.replacePersistentDatabase(candidate, 'unsafe-live-snapshot'))
            .rejects.toThrow('incomplete persistent working set')

        expect(captureRootValue).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('allows an explicitly authoritative complete replacement and publishes it normally', async () => {
        const database = makeDatabase()
        const candidate = makeDatabase()
        candidate.username = 'Imported complete database'
        const replaceDatabase = vi.fn()
        const store = {
            replaceFromDatabase: vi.fn(async () => ({ revision: 2 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
            isIncompleteWorkingSet: () => true,
        })
        coordinator.initialize(1)
        const initializedAuthorityEpoch = coordinator.storageAuthorityEpoch

        await coordinator.replacePersistentDatabase(candidate, 'explicit-import', {
            authoritative: true,
        })

        expect(store.replaceFromDatabase).toHaveBeenCalledWith(candidate, 1)
        expect(replaceDatabase).toHaveBeenCalledWith(candidate)
        expect(coordinator.revision).toBe(2)
        expect(coordinator.storageAuthorityEpoch).toBe(initializedAuthorityEpoch + 1)

        coordinator.initialize(2, candidate)
        expect(coordinator.storageAuthorityEpoch).toBe(initializedAuthorityEpoch + 2)
    })

    it('rejects a replacement whose expected revision is already stale', async () => {
        const database = makeDatabase()
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(6)

        await expect(coordinator.replacePersistentDatabase(database, 'stale-snapshot', {
            authoritative: true,
            expectedRevision: 5,
        })).rejects.toBeInstanceOf(RevisionConflictError)

        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('rejects a replacement whose snapshot mutation generation is stale before cloning', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const candidate = makeDatabase() as Database & { cloneTrap?: unknown }
            Object.defineProperty(candidate, 'cloneTrap', {
                enumerable: true,
                get() {
                    throw new Error('candidate was cloned')
                },
            })
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const store = makeStore(commit)
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)
            database.username = 'Later live edit'
            coordinator.markPersistentDataDirty(1)

            await expect(coordinator.replacePersistentDatabase(candidate, 'stale-snapshot', {
                authoritative: true,
                expectedRevision: 6,
                expectedMutationGeneration: 0,
            })).rejects.toThrow('mutation generation 0')

            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(expect.objectContaining({
                expectedRevision: 6,
                root: { username: 'Later live edit' },
            }))
        } finally {
            vi.useRealTimers()
        }
    })

    it('revalidates the snapshot mutation generation after async replacement preparation', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const prepared = deferred<Database>()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)

            const replacement = coordinator.replacePreparedPersistentDatabase(
                () => prepared.promise,
                'deferred-snapshot',
                {
                    authoritative: true,
                    expectedRevision: 6,
                    expectedMutationGeneration: 0,
                },
            )
            await Promise.resolve()
            database.username = 'Edit during preparation'
            coordinator.markPersistentDataDirty(1)
            prepared.resolve(makeDatabase())

            await expect(replacement).rejects.toThrow('mutation generation 0')
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })

    it('re-arms pending dirty data after a replacement store conflict', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const store = {
                commit,
                replaceFromDatabase: vi.fn().mockRejectedValue(
                    new RevisionConflictError(6, 7),
                ),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)
            database.username = 'Retryable edit'
            coordinator.markPersistentDataDirty(1)

            await expect(coordinator.replacePersistentDatabase(
                makeDatabase(),
                'conflicting-replacement',
            )).rejects.toBeInstanceOf(RevisionConflictError)

            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(expect.objectContaining({
                expectedRevision: 6,
                root: { username: 'Retryable edit' },
            }))
        } finally {
            vi.useRealTimers()
        }
    })

    it('commits a stable-ID character detail mutation without replacing its conversations', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a', 'unrelated']
        const committed = deferred<{ revision: number }>()
        const store = {
            commit: vi.fn(() => committed.promise),
            readRoot: vi.fn(async () => ({
                revision: 4,
                value: captureRoot(database),
            })),
            readCharacter: vi.fn(async () => ({
                revision: 4,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(4)

        const mutation = coordinator.mutatePersistentCharacterDetail(
            'char-a',
            'trash-character',
            ({ root, character }) => {
                character.trashTime = 123
                root.characterOrder = root.characterOrder.filter((entry) => entry !== 'char-a')
            },
        )
        await vi.waitFor(() => expect(store.commit).toHaveBeenCalledOnce())

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            root: expect.objectContaining({ characterOrder: ['unrelated'] }),
            character: expect.objectContaining({
                chaId: 'char-a',
                trashTime: 123,
            }),
        })
        expect(vi.mocked(store.commit).mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(publishCharacterMutation).not.toHaveBeenCalled()

        committed.resolve({ revision: 5 })
        await expect(mutation).resolves.toBe(true)

        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 5,
            characterId: 'char-a',
            kind: 'detail',
            character: expect.objectContaining({ trashTime: 123 }),
        }))
    })

    it('replaces exactly one complete character from authoritative detail and conversations', async () => {
        const database = makeDatabase()
        const storedChat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'preserved' }],
        }
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 7,
                value: { type: 'character', chaId: 'char-a', name: 'Cold stub' },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 7,
                items: [{
                    id: 'chat-a',
                    characterId: 'char-a',
                    name: 'Stored chat',
                    configuredIndex: 0,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 7, value: storedChat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(7)

        await expect(coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'cold-character-restore',
            (current) => ({ ...current, name: 'Restored' }),
        )).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                name: 'Restored',
                chats: [storedChat],
            }),
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 8,
            characterId: 'char-a',
            kind: 'replace',
            character: expect.objectContaining({ chats: [storedChat] }),
        }))
    })

    it('does not publish a stable-ID character mutation when its commit fails', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn().mockRejectedValue(new Error('character commit failed')),
            readRoot: vi.fn(async () => ({ revision: 9, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 9,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(9)

        await expect(coordinator.mutatePersistentCharacterDetail(
            'char-a',
            'delete-character',
            () => ({ delete: true }),
        )).rejects.toThrow('character commit failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(9)
    })

    it('pages group details and commits permanent deletion with every changed group once', async () => {
        const groupA = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['char-a', 'char-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [],
        } as groupChat
        const groupB = {
            ...structuredClone(groupA),
            chaId: 'group-b',
            characters: ['char-b', 'char-a'],
            characterTalks: [0.4, 0.6],
            characterActive: [true, false],
        } as groupChat
        const groupTrash = {
            ...structuredClone(groupA),
            chaId: 'group-trash',
            characters: ['char-a'],
            characterTalks: [0.9],
            characterActive: [true],
            trashTime: 100,
        } as groupChat
        const unreferenced = {
            ...structuredClone(groupA),
            chaId: 'group-unreferenced',
            characters: ['char-b'],
            characterTalks: [0.7],
            characterActive: [true],
        } as groupChat
        const target = makeDatabase().characters[0]
        const database = {
            ...makeDatabase(),
            characterOrder: ['group-a', 'char-a', 'group-b', 'group-trash'],
            characters: [groupA, groupB, groupTrash, unreferenced, target],
        } as Database
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const readCharacter = vi.fn(async (id: string) => ({
            revision: 1,
            value: structuredClone(database.characters.find((character) => character.chaId === id)),
        }))
        const lease = {
            revision: 1,
            readRoot: vi.fn(async () => ({ revision: 1, value: captureRoot(database) })),
            queryCharacters: vi.fn(async ({ trash, cursor }: { trash: boolean; cursor?: string }) => {
                if (trash) return {
                    revision: 1,
                    items: [{ id: 'group-trash', type: 'group' }],
                }
                if (!cursor) return {
                    revision: 1,
                    items: [
                        { id: 'group-a', type: 'group' },
                        { id: 'group-unreferenced', type: 'group' },
                    ],
                    nextCursor: 'next-page',
                }
                return {
                    revision: 1,
                    items: [
                        { id: 'group-b', type: 'group' },
                        { id: 'char-a', type: 'character' },
                    ],
                }
            }),
            readCharacter,
            release: vi.fn(async () => undefined),
        }
        const store = {
            commit,
            acquireRevision: vi.fn(async () => lease),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn((state) => {
            Object.assign(database, state.root)
            database.characters = database.characters.filter(
                (character) => character.chaId !== state.characterId,
            )
            for (const detail of state.relatedCharacters ?? []) {
                const live = database.characters.find(
                    (character) => character.chaId === detail.chaId,
                )
                if (live) Object.assign(live, detail)
            }
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'permanent-delete',
        )).resolves.toBe(true)

        expect(store.acquireRevision).toHaveBeenCalledWith(1)
        expect(lease.queryCharacters).toHaveBeenCalledTimes(3)
        expect(readCharacter.mock.calls.map(([id]) => id)).toEqual([
            'char-a',
            'group-a',
            'group-unreferenced',
            'group-b',
            'group-trash',
        ])
        expect(lease.release).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 1,
            root: expect.objectContaining({
                characterOrder: ['group-a', 'group-b', 'group-trash'],
            }),
            deleteCharacterId: 'char-a',
            characterDetails: [
                expect.objectContaining({
                    chaId: 'group-a',
                    characters: ['char-b'],
                    characterTalks: [0.75],
                    characterActive: [true],
                }),
                expect.objectContaining({
                    chaId: 'group-b',
                    characters: ['char-b'],
                    characterTalks: [0.4],
                    characterActive: [true],
                }),
                expect.objectContaining({
                    chaId: 'group-trash',
                    characters: [],
                    characterTalks: [],
                    characterActive: [],
                }),
            ],
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 2,
            characterId: 'char-a',
            kind: 'delete',
            relatedCharacters: expect.arrayContaining([
                expect.objectContaining({ chaId: 'group-a' }),
                expect.objectContaining({ chaId: 'group-b' }),
                expect.objectContaining({ chaId: 'group-trash' }),
            ]),
        }))
        expect(database.characters.map((character) => character.chaId)).toEqual([
            'group-a',
            'group-b',
            'group-trash',
            'group-unreferenced',
        ])
        expect(groupA.characters).toEqual(['char-b'])
        expect(groupB.characters).toEqual(['char-b'])
        expect(groupTrash.characters).toEqual([])
        expect(unreferenced.characters).toEqual(['char-b'])
    })

    it('adopts a selected related group baseline without a trailing full-character commit', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['char-a', 'char-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [],
        } as groupChat
        const target = makeDatabase().characters[0]
        const database = {
            ...makeDatabase(),
            characterOrder: ['group-a', 'char-a'],
            characters: [group, target],
        } as Database
        const lease = makeGroupDeletionLease(database)
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => group,
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => publishGroupDeletion(database, state),
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'selected-group-delete',
        )).resolves.toBe(true)
        await coordinator.flushPendingData('after-selected-group-delete')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 1,
            deleteCharacterId: 'char-a',
            characterDetails: [expect.objectContaining({
                chaId: 'group-a',
                characters: ['char-b'],
            })],
        })
        expect(group.characters).toEqual(['char-b'])
        expect(coordinator.revision).toBe(2)
    })

    it.each([true, false])(
        'preserves and follows up a pending-commit edit to a %s selected related group',
        async (selectedGroup) => {
            const group = {
                type: 'group',
                chaId: 'group-a',
                name: 'Group',
                additionalText: 'Initial',
                characters: ['char-a', 'char-b'],
                characterTalks: [0.25, 0.75],
                characterActive: [false, true],
                chats: [],
            } as groupChat
            const target = makeDatabase().characters[0]
            const other = {
                ...structuredClone(target),
                chaId: 'char-b',
                name: 'Beta',
            } as character
            const database = {
                ...makeDatabase(),
                characterOrder: ['group-a', 'char-a', 'char-b'],
                characters: [group, target, other],
            } as Database
            const lease = makeGroupDeletionLease(database)
            const atomicCommit = deferred<{ revision: number }>()
            const commit = vi.fn()
                .mockImplementationOnce(() => atomicCommit.promise)
                .mockImplementationOnce(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))
            const coordinator = new SaveCoordinator({
                store: {
                    acquireRevision: vi.fn(async () => lease),
                    commit,
                } as unknown as PersistentDataStore,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selectedGroup ? group : other,
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
                publishCharacterMutation: (state) => publishGroupDeletion(database, state),
            })
            coordinator.initialize(1)

            const deletion = coordinator.deletePersistentCharacterWithGroupReferences(
                'char-a',
                'pending-related-group-edit',
            )
            await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
            group.additionalText = 'Live edit while atomic commit is pending'
            coordinator.markPersistentDataDirty(1)
            atomicCommit.resolve({ revision: 2 })

            await expect(deletion).resolves.toBe(true)
            await coordinator.flushPendingData('after-related-compensation')

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toMatchObject({
                expectedRevision: 2,
                replaceCharacter: expect.objectContaining({
                    chaId: 'group-a',
                    additionalText: 'Live edit while atomic commit is pending',
                    characters: ['char-b'],
                    characterTalks: [0.75],
                    characterActive: [true],
                }),
            })
            expect(group.additionalText).toBe('Live edit while atomic commit is pending')
            expect(group.characters).toEqual(['char-b'])
            expect(coordinator.revision).toBe(3)
            expect(coordinator.pendingBytes).toBe(0)
        },
    )

    it('reconstructs selected group conversation stubs before pending-delete compensation', async () => {
        const selectedConversation = {
            id: 'selected-chat',
            name: 'Selected chat',
            message: [{ role: 'user', data: 'selected body', chatId: 'selected-message' }],
        } as groupChat['chats'][number]
        const omittedConversation = {
            id: 'omitted-chat',
            name: 'Omitted chat',
            note: 'Authoritative note',
            localLore: [{ key: 'authoritative lore', content: 'keep' }],
            message: [{ role: 'char', data: 'omitted body', chatId: 'omitted-message' }],
        } as groupChat['chats'][number]
        const omittedStub = createConversationSummaryStubFromChat(
            'group-a',
            omittedConversation,
            1,
        )
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            additionalText: 'Initial',
            characters: ['char-a', 'char-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [selectedConversation, omittedStub],
            chatPage: 0,
        } as groupChat
        const target = makeDatabase().characters[0]
        const database = {
            ...makeDatabase(),
            characterOrder: ['group-a', 'char-a'],
            characters: [group, target],
        } as Database
        const lease = makeGroupDeletionLease(database)
        const atomicCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => atomicCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const queryConversations = vi.fn(async () => ({
            revision: 2,
            items: [selectedConversation, omittedConversation].map((conversation, configuredIndex) => ({
                id: conversation.id!,
                characterId: 'group-a',
                name: conversation.name,
                configuredIndex,
                recentAt: 0,
                messageCount: conversation.message.length,
            })),
        }))
        const readConversation = vi.fn(async (_characterId: string, conversationId: string) => ({
            revision: 2,
            value: structuredClone(
                conversationId === selectedConversation.id
                    ? selectedConversation
                    : omittedConversation,
            ),
        }))
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
                queryConversations,
                readConversation,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => group,
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => publishGroupDeletion(database, state),
        })
        coordinator.initialize(1)

        const deletion = coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'pending-stubbed-group-edit',
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        group.additionalText = 'Live group edit'
        coordinator.markPersistentDataDirty(1)
        atomicCommit.resolve({ revision: 2 })

        await expect(deletion).resolves.toBe(true)
        await coordinator.flushPendingData('after-stubbed-group-compensation')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].replaceCharacter.chats).toEqual([
            selectedConversation,
            {
                ...omittedConversation,
                name: omittedStub.name,
                folderId: omittedStub.folderId,
                bindedPersona: omittedStub.bindedPersona,
                lastDate: omittedStub.lastDate,
            },
        ])
        expect(queryConversations).toHaveBeenCalledOnce()
        expect(readConversation).toHaveBeenCalledTimes(2)
        expect(coordinator.revision).toBe(3)
    })

    it.each(['stale-lease', 'read-failure'] as const)(
        'releases a failed permanent-delete lease and does not commit for %s',
        async (failure) => {
            const database = makeDatabase()
            const lease = makeGroupDeletionLease(database)
            if (failure === 'stale-lease') lease.revision = 0
            if (failure === 'read-failure') {
                lease.readRoot.mockRejectedValueOnce(new Error('read failed'))
            }
            const commit = vi.fn()
            const coordinator = new SaveCoordinator({
                store: {
                    acquireRevision: vi.fn(async () => lease),
                    commit,
                } as unknown as PersistentDataStore,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => null,
                captureCharacter: () => null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(1)

            await expect(coordinator.deletePersistentCharacterWithGroupReferences(
                'char-a',
                `failed-delete-${failure}`,
            )).rejects.toThrow()

            expect(lease.release).toHaveBeenCalledOnce()
            expect(commit).not.toHaveBeenCalled()
            expect(coordinator.revision).toBe(1)
        },
    )

    it('retries a transient permanent-delete lease release before committing', async () => {
        const database = makeDatabase()
        const lease = makeGroupDeletionLease(database)
        lease.release
            .mockRejectedValueOnce(new Error('release unavailable'))
            .mockResolvedValueOnce(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => publishGroupDeletion(database, state),
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'transient-release-delete',
        )).resolves.toBe(true)

        expect(lease.release).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledOnce()
    })

    it('preserves a permanent-delete read failure when both release attempts fail', async () => {
        const database = makeDatabase()
        const lease = makeGroupDeletionLease(database)
        const primaryError = new Error('read failed')
        lease.readRoot.mockRejectedValueOnce(primaryError)
        lease.release.mockRejectedValue(new Error('release unavailable'))
        const commit = vi.fn()
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'failed-read-and-release-delete',
        )).rejects.toBe(primaryError)

        expect(lease.release).toHaveBeenCalledTimes(2)
        expect(commit).not.toHaveBeenCalled()
    })

    it.each(['detail', 'replace', 'upsert'] as const)(
        'rejects an async %s mutation when its resident character changes before commit',
        async (operation) => {
            const database = makeDatabase()
            const mutationStarted = deferred<void>()
            const mutationGate = deferred<void>()
            const store = {
                commit: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            let mutation: Promise<boolean>
            if (operation === 'detail') {
                mutation = coordinator.mutatePersistentCharacterDetail(
                    'char-a',
                    'async-detail',
                    async ({ character }) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        character.name = 'Explicit detail'
                    },
                )
            } else if (operation === 'replace') {
                mutation = coordinator.replacePersistentCompleteCharacter(
                    'char-a',
                    'async-replace',
                    async (character) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        return { ...character, name: 'Explicit replacement' }
                    },
                )
            } else {
                mutation = coordinator.upsertPersistentCompleteCharacter(
                    'char-a',
                    'async-upsert',
                    async (character) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        return { ...character!, name: 'Explicit upsert' }
                    },
                )
            }
            await mutationStarted.promise
            ;(database.characters[0] as character).desc = 'Later resident edit'
            coordinator.markPersistentDataDirty(1)
            mutationGate.resolve()

            await expect(mutation).rejects.toThrow('Resident character changed')
            expect(store.commit).not.toHaveBeenCalled()
        },
    )

    it.each(['detail', 'replace', 'upsert'] as const)(
        'rejects a %s mutation when its resident character changes during authoritative reads',
        async (operation) => {
            const database = makeDatabase()
            const characterRead = deferred<{
                revision: number
                value: { type: 'character'; chaId: string; name: string }
            }>()
            const store = {
                commit: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(() => characterRead.promise),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            let mutation: Promise<boolean>
            if (operation === 'detail') {
                mutation = coordinator.mutatePersistentCharacterDetail(
                    'char-a',
                    'read-race-detail',
                    ({ character }) => {
                        character.name = 'Explicit detail'
                    },
                )
            } else if (operation === 'replace') {
                mutation = coordinator.replacePersistentCompleteCharacter(
                    'char-a',
                    'read-race-replace',
                    (character) => ({ ...character, name: 'Explicit replacement' }),
                )
            } else {
                mutation = coordinator.upsertPersistentCompleteCharacter(
                    'char-a',
                    'read-race-upsert',
                    (character) => ({ ...character!, name: 'Explicit upsert' }),
                )
            }
            await vi.waitFor(() => expect(store.readCharacter).toHaveBeenCalledOnce())
            ;(database.characters[0] as character).desc = 'Edit during authoritative read'
            coordinator.markPersistentDataDirty(1)
            characterRead.resolve({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })

            await expect(mutation).rejects.toThrow('Resident character changed')
            expect(store.commit).not.toHaveBeenCalled()
        },
    )

    it('compensates a resident edit made while a complete-character commit is pending', async () => {
        const database = makeDatabase()
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) =>
                database.characters.find((character) => character.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(10)

        const mutation = coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'pending-replace',
            (character) => ({ ...character, name: 'Explicit replacement' }),
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        ;(database.characters[0] as character).desc = 'Later resident edit'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 11 })

        await expect(mutation).rejects.toThrow('Resident character changed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 11,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                name: 'Alpha',
                desc: 'Later resident edit',
            }),
        })
        expect(coordinator.revision).toBe(12)
    })

    it('bounds resident compensation and leaves continuous edits for a trailing flush', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let coordinator!: SaveCoordinator
            const commit = vi.fn(async ({ expectedRevision }) => {
                const call = commit.mock.calls.length
                if (call > 4) throw new Error('unbounded compensation flush entered')
                ;(database.characters[0] as character).desc = `Concurrent edit ${call}`
                coordinator.markPersistentDataDirty(1)
                return { revision: expectedRevision + 1 }
            })
            const store = {
                commit,
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            await expect(coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'continuous-resident-edits',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )).rejects.toThrow('Resident character changed')

            expect(commit).toHaveBeenCalledTimes(4)
            expect(coordinator.revision).toBe(14)
            expect(coordinator.pendingBytes).toBe(1)
            expect(vi.getTimerCount()).toBeGreaterThan(0)
        } finally {
            vi.useRealTimers()
        }
    })

    it('keeps a continuously edited non-selected resident target dirty after compensation', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const selected = structuredClone(database.characters[0])
            selected.chaId = 'char-b'
            selected.name = 'Beta'
            database.characters.push(selected)
            let coordinator!: SaveCoordinator
            const commit = vi.fn(async ({ expectedRevision }) => {
                const call = commit.mock.calls.length
                if (call <= 4) {
                    ;(database.characters[0] as character).desc = `Concurrent inactive edit ${call}`
                    coordinator.markPersistentDataDirty(1)
                }
                return { revision: expectedRevision + 1 }
            })
            const store = {
                commit,
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[1],
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            await expect(coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'continuous-inactive-resident-edits',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )).rejects.toThrow('Resident character changed')

            expect(commit).toHaveBeenCalledTimes(4)
            expect(coordinator.pendingBytes).toBe(1)
            expect(vi.getTimerCount()).toBeGreaterThan(0)

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledTimes(5)
            expect(commit.mock.calls[4][0]).toMatchObject({
                expectedRevision: 14,
                replaceCharacter: expect.objectContaining({
                    chaId: 'char-a',
                    desc: 'Concurrent inactive edit 4',
                }),
            })
            expect(coordinator.revision).toBe(15)
            expect(coordinator.pendingBytes).toBe(0)
        } finally {
            vi.useRealTimers()
        }
    })

    it('defers a local-only resident compensation revision for official publication', async () => {
        const database = makeDatabase()
        const target = database.characters[0] as character
        target.chatPage = 0
        target.chats = [{
            id: 'chat-a',
            name: 'Chat',
            note: '',
            localLore: [],
            message: [],
        }]
        const selected = structuredClone(target)
        selected.chaId = 'char-b'
        selected.name = 'Beta'
        selected.chats[0].id = 'chat-b'
        database.characters.push(selected)
        let coordinator!: SaveCoordinator
        const commit = vi.fn(async ({ expectedRevision }) => {
            const call = commit.mock.calls.length
            if (call <= 4) {
                target.chats[0].message = [{
                    role: 'char',
                    data: `Completed generation ${call}`,
                    chatId: 'generation-message',
                }]
                coordinator.markPersistentDataDirty(1)
            }
            return { revision: expectedRevision + 1 }
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
        } as unknown as PersistentDataStore
        const publishedRevisions: number[] = []
        const laterPublication = deferred<void>()
        const pin = vi.fn(async (revision: number) => {
            if (revision !== 14) await laterPublication.promise
            return {
                publish: async () => {
                    publishedRevisions.push(revision)
                },
                dispose: async () => undefined,
            }
        })
        coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[1],
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            officialPublisher: { pin },
            clock: {
                setTimeout: () => Symbol('timer'),
                clearTimeout: () => undefined,
            },
        })
        coordinator.initialize(10)

        await expect(coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'continuous-generation-edit',
            (current) => ({ ...current, name: 'Explicit replacement' }),
        )).rejects.toThrow('Resident character changed')
        expect(publishedRevisions).toEqual([14])

        const result = await Promise.race([
            coordinator.flushPendingDataLocally('generation-completion').then(() => 'committed'),
            new Promise<string>((resolve) => setTimeout(() => resolve('blocked'), 25)),
        ])

        expect(result).toBe('committed')
        expect(commit).toHaveBeenCalledTimes(5)
        expect(commit.mock.calls[4][0]).toMatchObject({
            expectedRevision: 14,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                chats: [expect.objectContaining({
                    message: [expect.objectContaining({
                        data: 'Completed generation 4',
                    })],
                })],
            }),
        })
        expect(coordinator.revision).toBe(15)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
        expect(pin).toHaveBeenCalledOnce()

        const publishing = coordinator.publishCurrentOfficialRevision()
        await vi.waitFor(() => expect(pin).toHaveBeenLastCalledWith(15))
        laterPublication.resolve(undefined)
        await publishing
        expect(publishedRevisions).toEqual([14, 15])
    })

    it('publishes a deferred target compensation when a tokened replacement then rejects', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const selected = structuredClone(database.characters[0])
            selected.chaId = 'char-b'
            selected.name = 'Beta'
            database.characters.push(selected)
            let coordinator!: SaveCoordinator
            const commit = vi.fn(async ({ expectedRevision }) => {
                const call = commit.mock.calls.length
                if (call <= 4) {
                    ;(database.characters[0] as character).desc = `Concurrent inactive edit ${call}`
                    coordinator.markPersistentDataDirty(1)
                }
                return { revision: expectedRevision + 1 }
            })
            const store = {
                commit,
                replaceFromDatabase: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const publishedRevisions: number[] = []
            const pin = vi.fn(async (revision: number) => ({
                publish: async () => {
                    publishedRevisions.push(revision)
                },
                dispose: async () => undefined,
            }))
            coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[1],
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
            })
            coordinator.initialize(10)

            await expect(coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'continuous-inactive-resident-edits',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )).rejects.toThrow('Resident character changed')
            expect(publishedRevisions).toEqual([14])

            await expect(coordinator.replacePersistentDatabase(
                makeDatabase(),
                'stale-tokened-replacement',
                { authoritative: true, expectedRevision: 14 },
            )).rejects.toBeInstanceOf(RevisionConflictError)

            expect(commit).toHaveBeenCalledTimes(5)
            expect(coordinator.revision).toBe(15)
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            expect(publishedRevisions).toEqual([14])

            await vi.advanceTimersByTimeAsync(3_000)

            expect(pin).toHaveBeenLastCalledWith(15)
            expect(publishedRevisions).toEqual([14, 15])
        } finally {
            vi.useRealTimers()
        }
    })

    it('materializes a detached authoritative snapshot without publishing it', async () => {
        const database = makeDatabase()
        const snapshot = makeDatabase()
        snapshot.username = 'Authoritative snapshot'
        const store = {
            materializeDatabase: vi.fn(async (revision?: number) => {
                expect(revision).toBe(12)
                return snapshot
            }),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(12)

        const materialized = await coordinator.materializePersistentDatabaseSnapshot(
            'explicit-compatibility-snapshot',
        )

        expect(materialized).toEqual(snapshot)
        expect(materialized).not.toBe(snapshot)
        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(12)
    })

    it('flushes and captures one atomic revision and mutation generation token', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        database.username = 'Flushed before pin'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.capturePersistentMutationToken(
            'sync-conflict-safety-export',
        )).resolves.toEqual({
            revision: 13,
            mutationGeneration: 1,
        })
    })

    it('rejects a stale destructive replacement token after flushing the newer live edit', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        const token = await coordinator.capturePersistentMutationToken('native-restore-start')
        database.username = 'Live edit during native parse'
        coordinator.markPersistentDataDirty(1)

        await expect(
            coordinator.acquireDestructiveReplacementFence(token),
        ).rejects.toThrow(/revision|mutation generation/i)

        expect(coordinator.revision).toBe(13)
        expect(() => coordinator.markPersistentDataDirty(1)).not.toThrow()
    })

    it('blocks ordinary saves until the owning destructive fence is released', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage ?? {},
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        const token = await coordinator.capturePersistentMutationToken('native-restore-start')
        const fence = await coordinator.acquireDestructiveReplacementFence(token)

        expect(() => coordinator.markPersistentDataDirty(1)).toThrow(
            /replacement is active/i,
        )
        expect(() => coordinator.flushPendingData('ordinary-save')).toThrow(
            /replacement is active/i,
        )
        expect(() => coordinator.replacePersistentDatabase(
            makeDatabase(),
            'ordinary-replacement',
            { authoritative: true },
        )).toThrow(/replacement is active/i)

        coordinator.initialize(13, database)
        expect(() => coordinator.markPersistentDataDirty(1)).not.toThrow()
        expect(coordinator.mutationGeneration).toBe(0)

        database.username = 'Edit after authoritative publication'
        expect(() => coordinator.markPersistentDataDirty(1)).not.toThrow()
        expect(coordinator.mutationGeneration).toBe(1)
        expect(() => coordinator.flushPendingData('ordinary-save')).toThrow(
            /replacement is active/i,
        )

        coordinator.releaseDestructiveReplacementFence(fence)
        await coordinator.flushPendingData('post-fence-edit')
        expect(store.commit).toHaveBeenCalledWith(expect.objectContaining({
            expectedRevision: 13,
            root: expect.objectContaining({
                username: 'Edit after authoritative publication',
            }),
        }))
    })

    it('admits an already-applied edit while the exact fence is still acquiring', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage ?? {},
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        const token = await coordinator.capturePersistentMutationToken('native-restore-start')

        const acquiring = coordinator.acquireDestructiveReplacementFence(token)
        expect(() => coordinator.mutatePersistentPresets(
            'queued-preset-mutation',
            () => undefined,
        )).toThrow(/replacement is active/i)
        database.username = 'Edit completed during final handshake'
        expect(() => coordinator.markPersistentDataDirty(2 * 1024 * 1024)).not.toThrow()

        await expect(acquiring).rejects.toThrow(/revision|mutation generation/i)
        expect(coordinator.revision).toBe(13)
        expect(database.username).toBe('Edit completed during final handshake')
    })

    it('returns the snapshot revision atomically before a queued replacement advances it', async () => {
        const database = makeDatabase()
        const snapshot = makeDatabase()
        snapshot.username = 'Revision 12 snapshot'
        const store = {
            materializeDatabase: vi.fn(async () => snapshot),
            replaceFromDatabase: vi.fn(async () => ({ revision: 13 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)

        const materializing = coordinator.materializePersistentDatabaseSnapshotWithRevision(
            'versioned-snapshot',
        )
        const replacing = coordinator.replacePersistentDatabase(
            makeDatabase(),
            'queued-replacement',
            { authoritative: true },
        )

        await expect(materializing).resolves.toEqual({
            revision: 12,
            mutationGeneration: 0,
            database: snapshot,
        })
        await replacing
        expect(coordinator.revision).toBe(13)
    })

    it('reads one detached complete character without publishing it', async () => {
        const database = makeDatabase()
        const chat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readCharacter: vi.fn(async () => ({
                revision: 13,
                value: { type: 'character', chaId: 'char-a', name: 'Stored detail' },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 13,
                items: [{
                    id: 'chat-a',
                    characterId: 'char-a',
                    name: 'Stored chat',
                    configuredIndex: 0,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 13, value: chat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(13)

        const character = await coordinator.readPersistentCompleteCharacter(
            'char-a',
            'mcp-character-read',
        )

        expect(character).toEqual(expect.objectContaining({
            chaId: 'char-a',
            chats: [chat],
        }))
        expect(publishCharacterMutation).not.toHaveBeenCalled()
    })

    it('reads one detached authoritative conversation without changing selection', async () => {
        const database = makeDatabase()
        const chat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readConversation: vi.fn(async () => ({ revision: 17, value: chat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(17)

        const conversation = await coordinator.readPersistentConversation(
            'char-a',
            'chat-a',
            'mcp-conversation-read',
        )

        expect(conversation).toEqual(chat)
        expect(conversation).not.toBe(chat)
        expect(store.readConversation).toHaveBeenCalledWith('char-a', 'chat-a')
        expect(publishCharacterMutation).not.toHaveBeenCalled()
    })

    it('reads one ordered-position conversation when configured indexes have gaps', async () => {
        const database = makeDatabase()
        const chat = { id: 'chat-c', name: 'Third', note: '', localLore: [], message: [] }
        const store = {
            queryConversations: vi.fn(async () => ({
                revision: 18,
                items: [{
                    id: 'chat-c',
                    characterId: 'char-a',
                    name: 'Third',
                    configuredIndex: 7,
                    recentAt: 0,
                    messageCount: 0,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 18, value: chat })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(18)

        const conversation = await coordinator.readPersistentConversationAt(
            'char-a',
            2,
            'mcp-selected-conversation-read',
        )

        expect(conversation).toEqual(chat)
        expect(store.queryConversations).toHaveBeenCalledWith({
            characterId: 'char-a',
            order: 'configured',
            limit: 1,
            cursor: '2',
        })
        expect(store.readConversation).toHaveBeenCalledOnce()
    })

    it('reads character detail and its selected conversation in one serialized revision', async () => {
        const database = makeDatabase()
        const firstCharacterRead = deferred<{
            revision: number
            value: Database['characters'][number]
        }>()
        const character = {
            ...database.characters[0],
            chatPage: 1,
        }
        const chat = {
            id: 'chat-b',
            name: 'Selected',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readCharacter: vi.fn()
                .mockImplementationOnce(() => firstCharacterRead.promise)
                .mockResolvedValueOnce({ revision: 19, value: character }),
            queryConversations: vi.fn(async () => ({
                revision: 19,
                items: [{
                    id: 'chat-b',
                    characterId: 'char-a',
                    name: 'Selected',
                    configuredIndex: 4,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 19, value: chat })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(19)

        const selectedRead = coordinator.readPersistentSelectedConversation(
            'char-a',
            'mcp-selected-conversation-read',
        )
        await vi.waitFor(() => expect(store.readCharacter).toHaveBeenCalledOnce())
        const laterRead = coordinator.readPersistentCharacterDetail(
            'char-a',
            'queued-character-read',
        )
        firstCharacterRead.resolve({ revision: 19, value: character })

        const selected = await selectedRead
        await laterRead

        expect(selected).toEqual({ character, conversation: chat })
        expect(selected?.character).not.toBe(character)
        expect(selected?.conversation).not.toBe(chat)
        expect(store.queryConversations).toHaveBeenCalledWith({
            characterId: 'char-a',
            order: 'configured',
            limit: 1,
            cursor: '1',
        })
        expect(vi.mocked(store.queryConversations).mock.invocationCallOrder[0])
            .toBeLessThan(vi.mocked(store.readCharacter).mock.invocationCallOrder[1])
    })

    it('distinguishes an existing character without a selected conversation', async () => {
        const database = makeDatabase()
        const character = { ...database.characters[0], chatPage: 0 }
        const store = {
            readCharacter: vi.fn(async () => ({ revision: 20, value: character })),
            queryConversations: vi.fn(async () => ({ revision: 20, items: [] })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(20)

        await expect(coordinator.readPersistentSelectedConversation(
            'char-a',
            'mcp-empty-conversation-read',
        )).resolves.toEqual({
            character,
            conversation: null,
        })
    })

    it('returns null only when the selected character is missing', async () => {
        const database = makeDatabase()
        const store = {
            readCharacter: vi.fn(async () => null),
            queryConversations: vi.fn(),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(21)

        await expect(coordinator.readPersistentSelectedConversation(
            'missing',
            'mcp-missing-character-read',
        )).resolves.toBeNull()
        expect(store.queryConversations).not.toHaveBeenCalled()
    })

    it('returns an existing empty group with a null selected conversation', async () => {
        const database = makeDatabase()
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            chatPage: 0,
            chats: [],
            characters: [],
        } as unknown as Database['characters'][number]
        const store = {
            readCharacter: vi.fn(async () => ({ revision: 22, value: group })),
            queryConversations: vi.fn(async () => ({ revision: 22, items: [] })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(22)

        await expect(coordinator.readPersistentSelectedConversation(
            'group-a',
            'mcp-empty-group-read',
        )).resolves.toEqual({ character: group, conversation: null })
    })

    it('atomically adds an absent complete character before publishing it', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const added = {
            type: 'character',
            chaId: 'temp-char',
            name: 'Temporary',
            chats: [],
        } as Database['characters'][number]
        const store = {
            readRoot: vi.fn(async () => ({ revision: 14, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(14)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'temp-char',
            'multiuser-temp-character',
            (current) => {
                expect(current).toBeNull()
                return added
            },
        )).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 14,
            root: expect.objectContaining({ characterOrder: ['char-a', 'temp-char'] }),
            addCharacter: added,
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 15,
            characterId: 'temp-char',
            kind: 'add',
            character: added,
        }))
    })

    it('commits prepared character assets and their owner head before publishing the character', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const added = {
            type: 'character',
            chaId: 'prepared-char',
            name: 'Prepared',
            chats: [],
            additionalAssets: [['portrait', 'prepared/portrait.png', 'png']],
        } as Database['characters'][number]
        const assetAliases = [{
            kind: 'asset' as const,
            key: 'prepared/portrait.png',
            objectHash: 'a'.repeat(64),
            size: 4,
            mime: 'image/png',
            name: 'portrait.png',
            ext: 'png',
        }]
        const assetOwnerHeads = [{
            owner: {
                kind: 'character-additional-assets' as const,
                characterId: 'prepared-char',
            },
            present: true as const,
            manifestHash: 'b'.repeat(64),
            entryCount: 1,
        }]
        const events: string[] = []
        const store = {
            readRoot: vi.fn(async () => ({ revision: 23, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async () => {
                events.push('commit')
                expect(database.characters.map((item) => item.chaId)).toEqual(['char-a'])
                return { revision: 24 }
            }),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn((state) => {
            events.push('publish')
            database.characters.push(state.character as Database['characters'][number])
            Object.assign(database, state.root)
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(23)

        await coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => added,
            { assetAliases, assetOwnerHeads },
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 23,
            root: expect.objectContaining({ characterOrder: ['char-a', 'prepared-char'] }),
            addCharacter: added,
            assetAliases,
            assetOwnerHeads,
        })
        expect(events).toEqual(['commit', 'publish'])
        expect(database.characters.map((item) => item.chaId)).toEqual([
            'char-a',
            'prepared-char',
        ])
    })

    it('uses replacement instead of adding a duplicate complete character ID', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 15, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 15,
                value: { type: 'character', chaId: 'char-a', name: 'Existing' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 15, items: [] })),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(15)

        await coordinator.upsertPersistentCompleteCharacter(
            'char-a',
            'multiuser-existing-character',
            (current) => ({ ...current!, name: 'Updated' }),
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 15,
            replaceCharacter: expect.objectContaining({ chaId: 'char-a', name: 'Updated' }),
        })
        expect(vi.mocked(store.commit).mock.calls[0][0]).not.toHaveProperty('addCharacter')
    })

    it('does not publish an absent character when its atomic add fails', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 16, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn().mockRejectedValue(new Error('add failed')),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(16)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'temp-char',
            'multiuser-temp-character',
            () => ({ type: 'character', chaId: 'temp-char', name: 'Temporary', chats: [] } as any),
        )).rejects.toThrow('add failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(16)
    })

    it('leaves the working set unchanged when prepared character activation fails', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const before = structuredClone(database)
        const store = {
            readRoot: vi.fn(async () => ({ revision: 17, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn().mockRejectedValue(new Error('activation failed')),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(17)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => ({
                type: 'character',
                chaId: 'prepared-char',
                name: 'Prepared',
                chats: [],
                additionalAssets: [['portrait', 'prepared/portrait.png', 'png']],
            } as Database['characters'][number]),
            {
                assetAliases: [{
                    kind: 'asset',
                    key: 'prepared/portrait.png',
                    objectHash: 'a'.repeat(64),
                    size: 4,
                    mime: 'image/png',
                    name: 'portrait.png',
                    ext: 'png',
                }],
                assetOwnerHeads: [{
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: 'prepared-char',
                    },
                    present: true,
                    manifestHash: 'b'.repeat(64),
                    entryCount: 1,
                }],
            },
        )).rejects.toThrow('activation failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(database).toEqual(before)
        expect(coordinator.revision).toBe(17)
    })

    it('rejects an Inlay alias before prepared character activation', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 18, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async () => ({ revision: 19 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(18)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => ({
                type: 'character',
                chaId: 'prepared-char',
                name: 'Prepared',
                chats: [],
            } as Database['characters'][number]),
            {
                assetAliases: [{
                    kind: 'inlay',
                    key: 'prepared/portrait.png',
                    objectHash: 'a'.repeat(64),
                    size: 4,
                    mime: 'image/png',
                    name: 'portrait.png',
                    ext: 'png',
                    inlayType: 'image',
                }],
            } as any,
        )).rejects.toThrow('Prepared character aliases must be ordinary assets')

        expect(store.commit).not.toHaveBeenCalled()
    })

    it('rejects an owner head for another character before activation', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 20, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async () => ({ revision: 21 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(20)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => ({
                type: 'character',
                chaId: 'prepared-char',
                name: 'Prepared',
                chats: [],
            } as Database['characters'][number]),
            {
                assetOwnerHeads: [{
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: 'other-char',
                    },
                    present: true,
                    manifestHash: 'b'.repeat(64),
                    entryCount: 1,
                }],
            },
        )).rejects.toThrow(
            'Prepared character owner heads must belong to prepared-char',
        )

        expect(store.commit).not.toHaveBeenCalled()
    })

    it('can add an absent sentinel without adding it to character order', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const store = {
            readRoot: vi.fn(async () => ({ revision: 19, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(19)

        await coordinator.upsertPersistentCompleteCharacter(
            '§temp',
            'multiuser-sentinel',
            () => ({ type: 'character', chaId: '§temp', name: 'Temporary', chats: [] } as any),
            { includeInCharacterOrder: false },
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 19,
            addCharacter: expect.objectContaining({ chaId: '§temp' }),
        })
    })

    it('rebases concurrent live root edits while reading complete presets', async () => {
        const database = makeDatabase()
        database.botPresetsId = 0
        const presetRead = deferred<{
            revision: number
            value: Database['botPresets'][number]
        }>()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 6, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 6,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(() => presetRead.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(6)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.botPresetsId = 1
        })
        await vi.waitFor(() => expect(store.readPreset).toHaveBeenCalledOnce())
        database.username = 'Concurrent root edit'
        coordinator.markPersistentDataDirty(1)
        presetRead.resolve({ revision: 6, value: { name: 'First' } as Database['botPresets'][number] })
        await mutation

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 6,
            root: expect.objectContaining({
                botPresetsId: 1,
                username: 'Concurrent root edit',
            }),
            replacePresets: [{ name: 'First' }],
        })
        expect(database.username).toBe('Concurrent root edit')
    })

    it('preserves a same-field live root edit made during preset reads', async () => {
        const database = makeDatabase()
        database.mainPrompt = 'Initial prompt'
        const presetRead = deferred<{
            revision: number
            value: Database['botPresets'][number]
        }>()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 8, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 8,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(() => presetRead.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(8)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.mainPrompt = 'Preset prompt'
        })
        await vi.waitFor(() => expect(store.readPreset).toHaveBeenCalledOnce())
        database.mainPrompt = 'Later live prompt'
        coordinator.markPersistentDataDirty(1)
        presetRead.resolve({
            revision: 8,
            value: { name: 'First' } as Database['botPresets'][number],
        })
        await mutation

        expect(store.commit).toHaveBeenCalledWith(expect.objectContaining({
            expectedRevision: 8,
            root: expect.objectContaining({ mainPrompt: 'Later live prompt' }),
        }))
        expect(database.mainPrompt).toBe('Later live prompt')
    })

    it('preserves and later flushes live root edits made while a preset commit is pending', async () => {
        const database = makeDatabase()
        database.botPresetsId = 0
        const presetCommit = deferred<{ revision: number }>()
        const commit = vi.fn(({ expectedRevision }: { expectedRevision: number }) => {
            if (commit.mock.calls.length === 1) return presetCommit.promise
            return Promise.resolve({ revision: expectedRevision + 1 })
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 6, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 6,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 6,
                value: { name: 'First' },
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(6)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.botPresetsId = 1
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.username = 'Edit during preset commit'
        coordinator.markPersistentDataDirty(1)
        presetCommit.resolve({ revision: 7 })
        await mutation

        expect(database).toMatchObject({
            botPresetsId: 1,
            username: 'Edit during preset commit',
        })
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 6,
            root: {
                botPresetsId: 1,
                username: 'Fixture',
            },
        })

        await coordinator.flushPendingData('after-preset-commit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 7,
            root: {
                botPresetsId: 1,
                username: 'Edit during preset commit',
            },
        })
        expect(commit.mock.calls[1][0]).not.toHaveProperty('replacePresets')
        expect(coordinator.revision).toBe(8)
    })

    it.each([
        ['scalable', false],
        ['maximum', true],
    ])('preserves a later same-field root edit during a pending %s preset commit', async (
        _profile,
        capturesCompletePresets,
    ) => {
        const database = makeDatabase()
        database.mainPrompt = 'initial'
        database.botPresets = [{ name: 'First' }] as Database['botPresets']
        const presetCommit = deferred<{ revision: number }>()
        const commit = vi.fn(({ expectedRevision }: { expectedRevision: number }) => {
            if (commit.mock.calls.length === 1) return presetCommit.promise
            return Promise.resolve({ revision: expectedRevision + 1 })
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 30, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 30,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 30,
                value: { name: 'First' },
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => capturesCompletePresets ? database.botPresets : null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(30)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.mainPrompt = 'preset selection'
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.mainPrompt = 'later user edit'
        coordinator.markPersistentDataDirty(1)
        presetCommit.resolve({ revision: 31 })
        await mutation

        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 30,
            root: expect.objectContaining({ mainPrompt: 'preset selection' }),
        })
        expect(database.mainPrompt).toBe('later user edit')

        await coordinator.flushPendingData('after-preset-same-field-edit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 31,
            root: expect.objectContaining({ mainPrompt: 'later user edit' }),
        })
        expect(coordinator.revision).toBe(32)
    })

    it('does not commit when the persistent working copy is clean', async () => {
        const database = makeDatabase()
        const store = { commit: vi.fn() } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)

        await coordinator.flushPendingData('test')

        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(4)
    })

    it('captures root and the selected character without traversing inactive characters', async () => {
        const database = makeDatabase()
        const inactive = makeDatabase().characters[0]
        Object.defineProperty(inactive, 'chats', {
            enumerable: true,
            get: () => {
                throw new Error('inactive character was traversed')
            },
        })
        database.characters.push(inactive)
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })

        coordinator.initialize(1)
        await coordinator.flushPendingData('clean')
    })

    it('initializes baselines from the supplied authoritative database', async () => {
        let database = makeDatabase()
        database.username = 'Stale live state'
        const authoritative = makeDatabase()
        authoritative.username = 'Authoritative state'
        authoritative.characters[0].name = 'Authoritative character'
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
        })

        coordinator.initialize(6, authoritative)
        database = structuredClone(authoritative)
        await coordinator.flushPendingData('clean')

        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(6)
    })

    it('returns the exact shared promise for concurrent flushes', async () => {
        const database = makeDatabase()
        const pending = deferred<{ revision: number }>()
        const store = makeStore(vi.fn(() => pending.promise))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(10)

        const first = coordinator.flushPendingData('first')
        const second = coordinator.flushPendingData('second')

        expect(second).toBe(first)
        pending.resolve({ revision: 2 })
        await first
    })

    it('reports the exact shared flush promise and clears it after completion', async () => {
        const database = makeDatabase()
        const pending = deferred<{ revision: number }>()
        const reported: Array<Promise<void> | null> = []
        const coordinator = new SaveCoordinator({
            store: makeStore(vi.fn(() => pending.promise)),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
        })
        coordinator.initialize(1)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(1)

        const first = coordinator.flushPendingData('first')
        const second = coordinator.flushPendingData('second')
        pending.resolve({ revision: 2 })
        await first
        await Promise.resolve()

        expect(second).toBe(first)
        expect(reported).toEqual([first, null])
    })

    it('debounces for 500 ms and flushes immediately at the pending byte limit', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Debounced'

            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(499)
            expect(store.commit).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(1)
            expect(store.commit).toHaveBeenCalledTimes(1)

            database.username = 'Immediate'
            coordinator.markPersistentDataDirty(1_048_576)
            await vi.advanceTimersByTimeAsync(0)
            expect(store.commit).toHaveBeenCalledTimes(2)
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).toHaveBeenCalledTimes(2)
        } finally {
            vi.useRealTimers()
        }
    })

    it('treats byte estimates as absolute pending size instead of summing them', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Large snapshots'

            coordinator.markPersistentDataDirty(600_000)
            coordinator.markPersistentDataDirty(600_000)

            expect(coordinator.pendingBytes).toBe(600_000)
            await vi.advanceTimersByTimeAsync(0)
            expect(store.commit).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).toHaveBeenCalledTimes(1)
        } finally {
            vi.useRealTimers()
        }
    })

    it('starts one immediate flush while estimates stay above the byte limit', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const gate = deferred<{ revision: number }>()
            const commit = vi.fn().mockImplementationOnce(() => gate.promise)
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Huge'

            coordinator.markPersistentDataDirty(2_097_152)
            await vi.advanceTimersByTimeAsync(0)
            expect(commit).toHaveBeenCalledTimes(1)
            coordinator.markPersistentDataDirty(2_097_152)
            coordinator.markPersistentDataDirty(2_097_152)
            await vi.advanceTimersByTimeAsync(0)
            expect(commit).toHaveBeenCalledTimes(1)

            gate.resolve({ revision: 2 })
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledTimes(1)
            expect(coordinator.pendingBytes).toBe(0)
        } finally {
            vi.useRealTimers()
        }
    })

    it('clamps invalid estimated byte counts to zero while retaining dirty work', async () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        coordinator.markPersistentDataDirty(-1)
        coordinator.markPersistentDataDirty(Number.NaN)
        coordinator.markPersistentDataDirty(Number.POSITIVE_INFINITY)

        expect(coordinator.pendingBytes).toBe(0)
        await coordinator.flushPendingData('cleanup')
    })

    it('commits only changed root and complete selected-character fields', async () => {
        const database = makeDatabase()
        database.characters[0].chats = [
            { id: 'one', name: 'One', message: [], localLore: [], note: '' },
            { id: 'two', name: 'Two', message: [], localLore: [], note: '' },
        ]
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = makeStore(commit)
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.username = 'Root changed'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('root')
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 2,
            root: { username: 'Root changed' },
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')

        database.characters[0].chats[1].name = 'Inactive renamed'
        database.characters[0].chats.reverse()
        database.characters[0].chats.push({
            id: 'three',
            name: 'Three',
            message: [],
            localLore: [],
            note: '',
        })
        database.characters[0].chats.splice(1, 1)
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('character')
        expect(commit.mock.calls[1][0]).not.toHaveProperty('root')
        expect(commit.mock.calls[1][0].replaceCharacter.chats.map((chat: { id: string }) => chat.id)).toEqual([
            'two',
            'three',
        ])

        database.username = 'Combined root'
        database.characters[0].name = 'Combined character'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('combined')
        expect(commit.mock.calls[2][0]).toMatchObject({
            expectedRevision: 4,
            root: { username: 'Combined root' },
            replaceCharacter: { name: 'Combined character' },
        })
    })

    function makeChattyDatabase() {
        const database = makeDatabase()
        database.characters[0].chats = [
            {
                id: 'one',
                name: 'One',
                message: [{ role: 'user', data: 'hello one' }],
                localLore: [],
                note: '',
            },
            {
                id: 'two',
                name: 'Two',
                message: [
                    { role: 'user', data: 'hello two' },
                    { role: 'char', data: 'reply two' },
                ],
                localLore: [],
                note: '',
            },
        ]
        return database
    }

    it('commits session-owned replacement ranges in order and acknowledges the exact version', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.transaction((transaction) => {
            transaction.edit(
                transaction.locate(0),
                { role: 'user', data: 'session edit' },
            )
            transaction.append({ role: 'char', data: 'session append' })
        })
        await coordinator.flushPendingData('session-ranges')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].conversations).toEqual([
            {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 0,
                deleteCount: 1,
                messages: [{ role: 'user', data: 'session edit' }],
                conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
            },
            {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 2,
                deleteCount: 0,
                messages: [{ role: 'char', data: 'session append' }],
                conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
            },
        ])
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(onPersisted).toHaveBeenCalledWith(expect.objectContaining({
            characterId: 'char-a',
            conversationId: 'two',
            sessionVersion: 2,
            revision: 3,
        }))
    })

    it('persists one atomic multi-range and metadata operation through one working-set commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        const expectedMetadata = cloneConversationMetadata(conversation)

        session.applyOperation({
            expectedVersion: 0,
            expectedMetadata,
            metadata: {
                ...expectedMetadata,
                scriptstate: { '$counter': '2' },
            },
            ranges: [
                {
                    position: session.positionAt(0),
                    deleteCount: 1,
                    messages: [{ role: 'user', data: 'parsed first' }],
                },
                {
                    position: session.positionAt(1),
                    deleteCount: 1,
                    messages: [{ role: 'char', data: 'parsed second' }],
                },
            ],
        })
        await coordinator.flushPendingData('atomic-multi-range')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 2,
            conversations: [
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'two',
                    start: 0,
                    deleteCount: 1,
                    messages: [{ role: 'user', data: 'parsed first' }],
                    conversation: expect.objectContaining({
                        scriptstate: { '$counter': '2' },
                    }),
                },
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'two',
                    start: 1,
                    deleteCount: 1,
                    messages: [{ role: 'char', data: 'parsed second' }],
                    conversation: expect.objectContaining({
                        scriptstate: { '$counter': '2' },
                    }),
                },
            ],
        })
        expect(session.persistedVersion).toBe(2)
        expect(session.storeRevision).toBe(3)
    })

    it('commits and acknowledges an ordered session command whose final value is unchanged', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            maxResidentBytes: 0,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.edit(session.locate(0), { role: 'user', data: 'hello two' })
        await coordinator.flushPendingData('unchanged-session-command')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].conversations).toEqual([{
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            start: 0,
            deleteCount: 1,
            messages: [{ role: 'user', data: 'hello two' }],
            conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
        }])
        expect(session.persistedVersion).toBe(1)
        expect(session.storeRevision).toBe(3)
    })

    it('acknowledges a session append covered by legacy interaction and message-ID normalization', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const onPersisted = vi.fn((event) => {
            session.acknowledgePersisted(
                event.sessionToken,
                event.sessionVersion,
                event.revision,
            )
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        database.characters[0].lastInteraction = 123
        database.characters[0].chats[1].message =
            database.characters[0].chats[1].message.map((message, index) => {
                message.chatId ??= `normalized-${index}`
                return message
            })
        coordinator.markPersistentDataDirty(1)
        session.append({
            role: 'user',
            data: 'session append',
            chatId: 'session-output',
        })
        await coordinator.flushPendingData('mixed-session-and-legacy')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].conversations).toBeUndefined()
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            lastInteraction: 123,
            chats: [
                expect.anything(),
                {
                    id: 'two',
                    name: 'Two',
                    localLore: [],
                    note: '',
                    message: [
                        expect.objectContaining({ chatId: 'normalized-0' }),
                        expect.objectContaining({ chatId: 'normalized-1' }),
                        expect.objectContaining({
                            data: 'session append',
                            chatId: 'session-output',
                        }),
                    ],
                },
            ],
        })
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(session.persistedVersion).toBe(1)
        expect(session.storeRevision).toBe(3)

        session.append({
            role: 'char',
            data: 'second session append',
            chatId: 'second-session-output',
        })
        database.characters[0].name = 'Legacy follow-up'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('legacy-follow-up')
        expect(onPersisted).toHaveBeenCalledTimes(2)
        expect(session.persistedVersion).toBe(2)
        expect(session.storeRevision).toBe(4)
    })

    it('retains a session-owned range without acknowledgement until a failed save retries', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('range write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            maxResidentBytes: 0,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.append({ role: 'user', data: 'retry exact range' })
        await expect(coordinator.flushPendingData('first-attempt')).rejects.toThrow(
            'range write failed',
        )
        expect(session.persistedVersion).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.pinCount('dirty')).toBe(1)
        expect(session.residentBytes).toBe(1)
        expect(coordinator.pendingBytes).toBeGreaterThanOrEqual(0)

        await coordinator.flushPendingData('retry-attempt')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].conversations).toEqual(
            commit.mock.calls[0][0].conversations,
        )
        expect(session.persistedVersion).toBe(1)
        expect(session.storeRevision).toBe(3)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.pinCount('dirty')).toBe(0)
        expect(session.residentBytes).toBe(0)
    })

    it('pins only covered session commands while their store commit is in flight', async () => {
        const database = makeChattyDatabase()
        let finishCommit!: () => void
        const commitGate = new Promise<void>((resolve) => {
            finishCommit = resolve
        })
        const commit = vi.fn(async ({ expectedRevision }) => {
            await commitGate
            return { revision: expectedRevision + 1 }
        })
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            maxResidentBytes: 0,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.append({ role: 'user', data: 'pending exact range' })
        const flushing = coordinator.flushPendingData('pending-save-pin')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        expect(session.pinCount('dirty')).toBe(1)
        expect(session.pinCount('pending-save')).toBe(1)

        finishCommit()
        await flushing

        expect(session.pinCount('dirty')).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.persistedVersion).toBe(1)
    })

    it('retains then acknowledges a session command captured after a legacy structural append', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn()
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            .mockRejectedValueOnce(new Error('fallback write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        const baselineMessageCount = conversation.message.length
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation,
            storeRevision: 2,
            maxResidentBytes: 0,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        conversation.message.push({ role: 'char', data: 'legacy direct append' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('legacy-structural-baseline')

        session.append({ role: 'user', data: 'session append' })
        await expect(
            coordinator.flushPendingData('legacy-structural-fallback-failed'),
        ).rejects.toThrow('fallback write failed')

        expect(session.persistedVersion).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)

        await coordinator.flushPendingData('legacy-structural-fallback-retry')

        expect(session.residencyFallbackActive).toBe(true)
        expect(session.persistedVersion).toBe(1)
        expect(session.pinCount('dirty')).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.residentBytes).toBe(0)
        expect(commit).toHaveBeenCalledTimes(3)
        expect(commit.mock.calls[2][0].conversations).toEqual([
            expect.objectContaining({
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 0,
                deleteCount: baselineMessageCount + 1,
                messages: conversation.message,
            }),
        ])

        await coordinator.flushPendingData('legacy-structural-fallback-repeat')
        expect(commit).toHaveBeenCalledTimes(3)
    })

    it('keeps strict replacement evidence across a session-token rollover before flush', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        const makeSession = () => new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        makeSession().append({ role: 'user', data: 'first session' })
        makeSession().append({ role: 'char', data: 'replacement session' })
        await coordinator.flushPendingData('session-token-rollover')

        expect(commit.mock.calls[0][0].conversations).toEqual([
            expect.objectContaining({
                type: 'replace-range',
                start: 2,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'first session' }],
            }),
            expect.objectContaining({
                type: 'replace-range',
                start: 3,
                deleteCount: 0,
                messages: [{ role: 'char', data: 'replacement session' }],
            }),
        ])
        expect(onPersisted).toHaveBeenCalledTimes(2)
    })

    it('does not acknowledge pending evidence for a character omitted from the commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const detachedConversation: Chat = {
            id: 'detached-chat',
            name: 'Detached',
            message: [],
            localLore: [],
            note: '',
        }
        const detachedSession = new ActiveConversationSession({
            characterId: 'char-b',
            conversationId: 'detached-chat',
            conversation: detachedConversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        detachedSession.append({ role: 'user', data: 'not in selected capture' })
        database.characters[0].name = 'Committed selected character'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('other-character')

        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Committed selected character',
        })
        expect(onPersisted).not.toHaveBeenCalled()
        expect(detachedSession.persistedVersion).toBe(0)
    })

    it('does not acknowledge same-character evidence omitted from a fallback conversation commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const onPersistenceStarted = vi.fn(() => null)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: onPersistenceStarted,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const detachedConversation = structuredClone(database.characters[0].chats[1])
        const detachedSession = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: detachedConversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        detachedSession.append({ role: 'user', data: 'not in captured conversation' })
        database.characters[0].chats[0].message[0].data = 'captured fallback edit'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('same-character-fallback')

        expect(commit.mock.calls[0][0].conversations).toEqual([
            expect.objectContaining({
                characterId: 'char-a',
                conversationId: 'one',
            }),
        ])
        expect(onPersistenceStarted).not.toHaveBeenCalled()
        expect(onPersisted).not.toHaveBeenCalled()
    })

    it('acknowledges only the covered session prefix from a mixed fallback commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let coveredSession!: ActiveConversationSession
        let uncoveredSession!: ActiveConversationSession
        const onPersistenceStarted = vi.fn(() => null)
        const onPersisted = vi.fn((event) => {
            if (coveredSession.ownsSessionToken(event.sessionToken)) {
                coveredSession.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            } else if (uncoveredSession.ownsSessionToken(event.sessionToken)) {
                uncoveredSession.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            }
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: onPersistenceStarted,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const liveConversation = database.characters[0].chats[1]
        coveredSession = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: liveConversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        coveredSession.append({ role: 'user', data: 'covered append' })
        uncoveredSession = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: structuredClone(liveConversation),
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        uncoveredSession.append({ role: 'char', data: 'detached append' })
        database.characters[0].lastInteraction = 456
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('covered-prefix-fallback')

        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            lastInteraction: 456,
            chats: [
                expect.anything(),
                expect.objectContaining({
                    message: expect.arrayContaining([
                        expect.objectContaining({ data: 'covered append' }),
                    ]),
                }),
            ],
        })
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(onPersistenceStarted).toHaveBeenCalledOnce()
        expect(onPersistenceStarted).toHaveBeenCalledWith(expect.objectContaining({
            sessionToken: coveredSession.locate(2).sessionToken,
            sessionVersion: 1,
        }))
        expect(onPersisted).toHaveBeenCalledWith(expect.objectContaining({
            sessionToken: coveredSession.locate(2).sessionToken,
            sessionVersion: 1,
        }))
        expect(coveredSession.persistedVersion).toBe(1)
        expect(uncoveredSession.persistedVersion).toBe(0)
    })

    it.each([
        {
            label: 'tail edit',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message[1] = { role: 'char', data: 'edited reply' }
            },
            expected: {
                start: 1,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'edited reply' }],
            },
        },
        {
            label: 'middle insert',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message.splice(1, 0, { role: 'user', data: 'inserted' })
            },
            expected: {
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'inserted' }],
            },
        },
        {
            label: 'middle delete',
            prepare: (chat: Chat) => {
                chat.message.push({ role: 'user', data: 'shared suffix' })
            },
            mutate: (chat: Chat) => {
                chat.message.splice(1, 1)
            },
            expected: {
                start: 1,
                deleteCount: 1,
                messages: [],
            },
        },
        {
            label: 'complete replacement',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message.splice(
                    0,
                    chat.message.length,
                    { role: 'char', data: 'replacement one' },
                    { role: 'user', data: 'replacement two' },
                )
            },
            expected: {
                start: 0,
                deleteCount: 2,
                messages: [
                    { role: 'char', data: 'replacement one' },
                    { role: 'user', data: 'replacement two' },
                ],
            },
        },
        {
            label: 'metadata-only mutation',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.name = 'Renamed conversation'
            },
            expected: {
                start: 2,
                deleteCount: 0,
                messages: [],
            },
        },
    ])('commits the minimal conversation replace range for $label', async ({ prepare, mutate, expected }) => {
        const database = makeChattyDatabase()
        prepare(database.characters[0].chats[1])
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        mutate(database.characters[0].chats[1])
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-range')

        const mutation = commit.mock.calls[0][0].conversations[0]
        expect(mutation).toMatchObject({
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            ...expected,
        })
        expect(mutation.conversation).toEqual({
            id: 'two',
            name: database.characters[0].chats[1].name,
            localLore: [],
            note: '',
        })
    })

    it('preserves a shared suffix around a middle message edit', async () => {
        const database = makeChattyDatabase()
        database.characters[0].chats[1].message.push({ role: 'user', data: 'shared suffix' })
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message[1] = { role: 'char', data: 'middle edit' }
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-range')

        expect(commit.mock.calls[0][0].conversations[0]).toMatchObject({
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            start: 1,
            deleteCount: 1,
            messages: [{ role: 'char', data: 'middle edit' }],
        })
    })

    it('commits only the edited conversation when just chat content changes', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message.push({ role: 'user', data: 'follow-up' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(commit.mock.calls[0][0]).not.toHaveProperty('root')
        expect(commit.mock.calls[0][0].conversations).toEqual([
            {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 2,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'follow-up' }],
                conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
            },
        ])
        expect(coordinator.revision).toBe(3)

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(1)
    })

    it('replaces the whole character when a character field changes alongside chats', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].name = 'Alpha renamed'
        database.characters[0].chats[0].message.push({ role: 'char', data: 'more' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('detail-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({ name: 'Alpha renamed' })
    })

    it.each([
        ['added', (chats: { id?: string }[]) => chats.push({
            id: 'three',
            name: 'Three',
            message: [],
            localLore: [],
            note: '',
        } as never)],
        ['removed', (chats: { id?: string }[]) => chats.splice(0, 1)],
        ['reordered', (chats: { id?: string }[]) => chats.reverse()],
    ] as const)('replaces the whole character when chats are %s', async (_label, mutate) => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        mutate(database.characters[0].chats)
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('structure-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0].replaceCharacter.chats.map((chat: { id: string }) => chat.id))
            .toEqual(database.characters[0].chats.map((chat) => chat.id))

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(1)
    })

    it('replaces the whole character when a chat is missing an id', async () => {
        const database = makeChattyDatabase()
        delete database.characters[0].chats[1].id
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message.push({ role: 'user', data: 'anonymous' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('missing-id')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0]).toHaveProperty('replaceCharacter')
    })

    it('keeps conversation edits dirty when the mutation commit fails', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[0].message.push({ role: 'user', data: 'retry me' })
        coordinator.markPersistentDataDirty(5)
        await expect(coordinator.flushPendingData('fails')).rejects.toThrow('write failed')

        expect(coordinator.revision).toBe(2)
        expect(coordinator.pendingBytes).toBe(5)

        await coordinator.flushPendingData('retry')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].conversations).toMatchObject([
            {
                conversationId: 'one',
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'retry me' }],
            },
        ])
        expect(coordinator.revision).toBe(3)
    })

    it('reports a successful local revision once before official publication', async () => {
        const database = makeDatabase()
        const events: string[] = []
        const store = makeStore(vi.fn(async ({ expectedRevision }) => {
            events.push('commit')
            return { revision: expectedRevision + 1 }
        }))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onLocalRevision: (revision) => events.push(`local:${revision}`),
            officialPublisher: {
                pin: async () => ({
                    publish: async () => {
                        events.push('publish')
                    },
                    dispose: async () => undefined,
                }),
            },
        })
        coordinator.initialize(4)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('test')
        await coordinator.flushPendingData('clean')

        expect(events).toEqual(['commit', 'local:5', 'publish'])
    })

    it('installs and commits a live character addition with root and previous selected edits', async () => {
        const { database, added } = makeAdditionDatabase()
        const install = vi.fn(() => database.characters.push(added))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)
        database.username = 'Root changed'
        database.characters[0].name = 'Selected changed'

        await coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 123,
            install,
        }, 'new-character')

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 4,
            root: { username: 'Root changed' },
            replaceCharacter: { chaId: 'char-a', name: 'Selected changed' },
            addCharacter: { chaId: 'char-added', name: 'Added' },
        })
        expect(coordinator.revision).toBe(5)
    })

    it('reports a standalone addition exact promise and then idle state', async () => {
        const { database, added } = makeAdditionDatabase()
        const pending = deferred<{ revision: number }>()
        const reported: Array<Promise<void> | null> = []
        const coordinator = new SaveCoordinator({
            store: makeStore(vi.fn(() => pending.promise)),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
        })
        coordinator.initialize(1)

        const addition = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')

        expect(reported).toEqual([addition])
        pending.resolve({ revision: 2 })
        await addition
        await Promise.resolve()
        expect(reported).toEqual([addition, null])
    })

    it.each(['store', 'pin', 'publish'] as const)(
        'persists live character edits made while awaiting %s',
        async (stage) => {
            const { database, added } = makeAdditionDatabase()
            const storeGate = deferred<{ revision: number }>()
            const pinGate = deferred<{ publish(): Promise<void>; dispose(): Promise<void> }>()
            const publishGate = deferred<void>()
            const commit = vi.fn()
                .mockImplementationOnce(() => stage === 'store' ? storeGate.promise : Promise.resolve({ revision: 2 }))
                .mockResolvedValueOnce({ revision: 3 })
            const handle = {
                publish: vi.fn(() => stage === 'publish' ? publishGate.promise : Promise.resolve()),
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn(() => stage === 'pin' ? pinGate.promise : Promise.resolve(handle))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            const saving = coordinator.commitCharacterAddition({
                characterId: added.chaId,
                estimatedBytes: 1,
                install: () => database.characters.push(added),
            }, 'new-character')
            await vi.waitFor(() => {
                if (stage === 'store') expect(commit).toHaveBeenCalledOnce()
                if (stage === 'pin') expect(pin).toHaveBeenCalledOnce()
                if (stage === 'publish') expect(handle.publish).toHaveBeenCalledOnce()
            })
            added.name = `Changed during ${stage}`
            coordinator.markPersistentDataDirty(1)
            if (stage === 'store') storeGate.resolve({ revision: 2 })
            if (stage === 'pin') pinGate.resolve(handle)
            if (stage === 'publish') publishGate.resolve()
            await saving

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toMatchObject({
                expectedRevision: 2,
                replaceCharacter: { chaId: 'char-added', name: `Changed during ${stage}` },
            })
        },
    )

    it('serializes previous selected and added-character edits into separate trailing replacements', async () => {
        const { database, added } = makeAdditionDatabase()
        const firstPublish = deferred<void>()
        const publish = vi.fn()
            .mockImplementationOnce(() => firstPublish.promise)
            .mockResolvedValue(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: async () => ({ publish, dispose: vi.fn(async () => undefined) }),
            },
        })
        coordinator.initialize(1)
        const saving = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        database.characters[0].name = 'Selected during publish'
        added.name = 'Added during publish'
        firstPublish.resolve()
        await saving

        expect(commit).toHaveBeenCalledTimes(3)
        expect(commit.mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Selected during publish',
        })
        expect(commit.mock.calls[2][0].replaceCharacter).toMatchObject({
            chaId: 'char-added',
            name: 'Added during publish',
        })
    })

    it('publishes only the newest revision after addition edits committed while offline', async () => {
        const { database, added } = makeAdditionDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn(async (_revision: number) => handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('offline')
        added.name = 'Edited while offline'
        coordinator.markPersistentDataDirty(1)
        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(pin).toHaveBeenCalledTimes(2)
        expect(pin.mock.calls.map((call) => call[0])).toEqual([2, 3])
        expect(publish).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].replaceCharacter.name).toBe('Edited while offline')
    })

    it('gives an addition requested during an older failing publication its own serialized turn', async () => {
        const { database, added } = makeAdditionDatabase()
        const oldPublish = deferred<void>()
        const reported: Array<Promise<void> | null> = []
        const publish = vi.fn()
            .mockImplementationOnce(() => oldPublish.promise)
            .mockResolvedValue(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
            officialPublisher: {
                pin: async () => ({ publish, dispose: vi.fn(async () => undefined) }),
            },
        })
        coordinator.initialize(1)
        database.username = 'Older change'
        coordinator.markPersistentDataDirty(1)
        const olderFlush = coordinator.flushPendingData('older')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        const install = vi.fn(() => database.characters.push(added))
        const addition = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install,
        }, 'new-character')

        expect(addition).not.toBe(olderFlush)
        expect(reported).toEqual([olderFlush, addition])
        expect(install).not.toHaveBeenCalled()
        oldPublish.reject(new Error('older publication failed'))
        await expect(olderFlush).rejects.toThrow('older publication failed')
        await addition
        await Promise.resolve()

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(reported).toEqual([olderFlush, addition, null])
    })

    it('keeps an installed addition dirty after local failure for one explicit retry', async () => {
        const { database, added } = makeAdditionDatabase()
        const install = vi.fn(() => database.characters.push(added))
        const commit = vi.fn().mockRejectedValueOnce(new Error('write failed')).mockResolvedValueOnce({ revision: 2 })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 7,
            install,
        }, 'new-character')).rejects.toThrow('write failed')
        await coordinator.flushPendingData('retry')

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(coordinator.revision).toBe(2)
    })

    it('releases the reservation when install throws so later additions still run', async () => {
        const { database, added } = makeAdditionDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: 'char-broken',
            estimatedBytes: 1,
            install: () => {
                throw new Error('install failed')
            },
        }, 'broken')).rejects.toThrow('install failed')
        expect(commit).not.toHaveBeenCalled()

        await coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].addCharacter).toMatchObject({ chaId: 'char-added' })
    })

    it('surfaces the pending conflict to a later import instead of blocking it', async () => {
        const { database, added } = makeAdditionDatabase()
        const gate = deferred<{ revision: number }>()
        const conflict = new RevisionConflictError(1, 2)
        const commit = vi.fn(() => gate.promise)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        const first = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 9,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        gate.reject(conflict)
        await expect(first).rejects.toBe(conflict)
        expect(commit).toHaveBeenCalledOnce()

        const secondInstall = vi.fn()
        await expect(coordinator.commitCharacterAddition({
            characterId: 'char-other',
            estimatedBytes: 1,
            install: secondInstall,
        }, 'other-character')).rejects.toBe(conflict)

        expect(secondInstall).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(9)
    })

    it('lets a later import succeed after a transient addition failure', async () => {
        const { database, added } = makeAdditionDatabase()
        const other = structuredClone(added)
        other.chaId = 'char-other'
        other.name = 'Other'
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('write failed')

        await coordinator.commitCharacterAddition({
            characterId: other.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(other),
        }, 'other-character')

        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(commit.mock.calls[2][0].addCharacter).toMatchObject({ chaId: 'char-other' })
        expect(coordinator.revision).toBe(3)
    })

    it('defers a second import that starts while the first is still committing', async () => {
        const { database, added } = makeAdditionDatabase()
        const other = structuredClone(added)
        other.chaId = 'char-other'
        other.name = 'Other'
        const gate = deferred<{ revision: number }>()
        let commits = 0
        const commit = vi.fn(async () => {
            commits += 1
            return commits === 1 ? gate.promise : { revision: 3 }
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        const first = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 9,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        let installedSecond = false
        const second = coordinator.commitCharacterAddition({
            characterId: other.chaId,
            estimatedBytes: 1,
            install: () => {
                installedSecond = true
                database.characters.push(other)
            },
        }, 'other-character')
        expect(installedSecond).toBe(false)

        gate.resolve({ revision: 2 })
        await first
        await second

        expect(installedSecond).toBe(true)
        expect(database.characters.map((item) => item.chaId)).toContain('char-other')
    })

    it('runs an earlier replacement before installing an addition', async () => {
        let database = makeDatabase()
        const replacement = makeDatabase()
        replacement.username = 'Replacement'
        const replacementGate = deferred<{ revision: number }>()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = {
            commit,
            replaceFromDatabase: vi.fn(() => replacementGate.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => {
                database = structuredClone(candidate)
            },
        })
        coordinator.initialize(1)
        const replacing = coordinator.replacePersistentDatabase(replacement, 'replace')
        const install = vi.fn(() => {
            const added = structuredClone(database.characters[0])
            added.chaId = 'char-added'
            database.characters.push(added)
        })
        const adding = coordinator.commitCharacterAddition({
            characterId: 'char-added',
            estimatedBytes: 1,
            install,
        }, 'new-character')
        expect(install).not.toHaveBeenCalled()
        replacementGate.resolve({ revision: 2 })
        await replacing
        await adding

        expect(install).toHaveBeenCalledOnce()
        expect(database.username).toBe('Replacement')
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 2,
            addCharacter: { chaId: 'char-added' },
        })
    })

    it('successful replacement clears a failed addition and failed replacement preserves it', async () => {
        const { database, added } = makeAdditionDatabase()
        const commit = vi.fn().mockRejectedValue(new Error('addition failed'))
        const replaceFromDatabase = vi.fn()
            .mockRejectedValueOnce(new Error('replacement failed'))
            .mockResolvedValueOnce({ revision: 2 })
        const store = { commit, replaceFromDatabase } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => Object.assign(database, structuredClone(candidate)),
        })
        coordinator.initialize(1)
        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('addition failed')
        await expect(coordinator.replacePersistentDatabase(makeDatabase(), 'failed-replace')).rejects.toThrow('replacement failed')
        await expect(coordinator.flushPendingData('still-pending')).rejects.toThrow('addition failed')
        expect(commit).toHaveBeenCalledTimes(2)

        await coordinator.replacePersistentDatabase(makeDatabase(), 'successful-replace')
        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('authoritative replacement supersedes a locally added character after remote failure', async () => {
        let { database, added } = makeAdditionDatabase()
        const staleHandle = {
            publish: vi.fn().mockRejectedValue(new Error('offline')),
            dispose: vi.fn(async () => undefined),
        }
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = {
            commit,
            replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => {
                database = structuredClone(candidate)
            },
            officialPublisher: { pin: async () => staleHandle },
        })
        coordinator.initialize(1)
        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('offline')

        await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')
        await coordinator.flushPendingData('clean')

        expect(database.characters.map((character) => character.chaId)).toEqual(['char-a'])
        expect(commit).toHaveBeenCalledOnce()
        expect(staleHandle.dispose).toHaveBeenCalledOnce()
    })

    it('does not resurrect a character the replacement removed', async () => {
        vi.useFakeTimers()
        try {
            let db = makeDatabase()
            const replacementGate = deferred<{ revision: number }>()
            const store = {
                commit: vi.fn(),
                replaceFromDatabase: vi.fn(() => replacementGate.promise),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(db),
                captureSelectedCharacter: () => db.characters[0] ?? null,
                replaceDatabase: (replacement) => {
                    db = replacement
                },
            })
            coordinator.initialize(1)

            const candidate = makeDatabase()
            candidate.characters = []
            const replacing = coordinator.replacePersistentDatabase(candidate, 'remove-character')
            db.characters[0].name = 'Edited after enqueue'
            coordinator.markPersistentDataDirty(1)
            replacementGate.resolve({ revision: 2 })
            await replacing

            expect(db.characters).toEqual([])
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })

    it('reports a successful replacement revision and reports nothing on failure', async () => {
        let database = makeDatabase()
        const revisions: number[] = []
        const store = makeStore()
        vi.mocked(store.replaceFromDatabase)
            .mockResolvedValueOnce({ revision: 8 })
            .mockRejectedValueOnce(new Error('replace failed'))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
            onLocalRevision: (revision) => revisions.push(revision),
        })
        coordinator.initialize(7)

        await coordinator.replacePersistentDatabase(makeDatabase(), 'first')
        await expect(
            coordinator.replacePersistentDatabase(makeDatabase(), 'second'),
        ).rejects.toThrow('replace failed')

        expect(revisions).toEqual([8])
    })

    it('runs trailing commits for mutations made during every deferred commit', async () => {
        const database = makeDatabase()
        const first = deferred<{ revision: number }>()
        const second = deferred<{ revision: number }>()
        const commit = vi
            .fn()
            .mockImplementationOnce(() => first.promise)
            .mockImplementationOnce(() => second.promise)
            .mockResolvedValueOnce({ revision: 4 })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'one'
        coordinator.markPersistentDataDirty(1)
        const flushing = coordinator.flushPendingData('test')
        await Promise.resolve()
        database.username = 'two'
        coordinator.markPersistentDataDirty(1)
        first.resolve({ revision: 2 })
        await Promise.resolve()
        await Promise.resolve()
        database.username = 'three'
        coordinator.markPersistentDataDirty(1)
        second.resolve({ revision: 3 })

        await flushing

        expect(commit.mock.calls.map((call) => call[0].expectedRevision)).toEqual([1, 2, 3])
        expect(commit.mock.calls.map((call) => call[0].root.username)).toEqual(['one', 'two', 'three'])
    })

    it.each([
        new Error('write failed'),
        new RevisionConflictError(1, 2),
    ])('keeps failed work dirty without retrying for %s', async (error) => {
        const database = makeDatabase()
        const commit = vi.fn().mockRejectedValue(error)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'Uncommitted'
        coordinator.markPersistentDataDirty(25)

        await expect(coordinator.flushPendingData('test')).rejects.toBe(error)

        expect(commit).toHaveBeenCalledTimes(1)
        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(25)
    })

    it('commits locally even when the official publish fails and retries the pin later', async () => {
        const database = makeDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn().mockResolvedValue(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        database.username = 'Offline edit'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.flushPendingData('offline')).rejects.toThrow('offline')

        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(2)

        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(commit).toHaveBeenCalledOnce()
        expect(pin).toHaveBeenCalledTimes(1)
        expect(publish).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenCalledWith(2)
    })

    it('acknowledges a local generation commit without waiting for an unbounded official publisher', async () => {
        const database = makeDatabase()
        const scheduled: Array<() => void> = []
        const cleared = new Set<symbol>()
        const clock = {
            setTimeout: (callback: () => void) => {
                const handle = Symbol('timer')
                scheduled.push(() => {
                    if (!cleared.has(handle)) callback()
                })
                return handle
            },
            clearTimeout: (handle: unknown) => {
                if (typeof handle === 'symbol') cleared.add(handle)
            },
        }
        const pin = vi.fn(() => new Promise<never>(() => undefined))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            clock,
        })
        coordinator.initialize(1)
        database.username = 'Durable local generation'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingDataLocally('generation-completion')

        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(2)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
        expect(pin).not.toHaveBeenCalled()
        expect(scheduled.length).toBeGreaterThan(1)
    })

    it('does not let an in-flight unbounded publication block a newer local generation commit', async () => {
        const database = makeDatabase()
        const publish = vi.fn(() => new Promise<never>(() => undefined))
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'First local revision'
        coordinator.markPersistentDataDirty(1)

        void coordinator.flushPendingData('ordinary-save')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        database.username = 'Completed generation revision'
        coordinator.markPersistentDataDirty(1)

        const result = await Promise.race([
            coordinator.flushPendingDataLocally('generation-completion').then(() => 'committed'),
            new Promise<string>((resolve) => setTimeout(() => resolve('blocked'), 25)),
        ])

        expect(result).toBe('committed')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(coordinator.revision).toBe(3)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
        expect(pin).toHaveBeenCalledWith(2)
    })

    it('does not queue local acknowledgement behind a publication operation that has not started yet', async () => {
        const database = makeDatabase()
        const publish = vi.fn(() => new Promise<never>(() => undefined))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) })),
            },
        })
        coordinator.initialize(1)
        database.username = 'Completed generation'
        coordinator.markPersistentDataDirty(1)

        void coordinator.flushPendingData('ordinary-save')
        const result = await Promise.race([
            coordinator.flushPendingDataLocally('generation-completion').then(() => 'committed'),
            new Promise<string>((resolve) => setTimeout(() => resolve('blocked'), 25)),
        ])

        expect(result).toBe('committed')
        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(2)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
    })

    it('does not retry stalled official publication cleanup during local acknowledgement', async () => {
        const database = makeDatabase()
        const cleanupFailure = new Error('cleanup offline')
        const dispose = vi.fn()
            .mockRejectedValueOnce(cleanupFailure)
            .mockImplementationOnce(() => new Promise<never>(() => undefined))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: vi.fn(async () => ({
                    publish: vi.fn(async () => undefined),
                    dispose,
                })),
            },
            clock: {
                setTimeout: () => Symbol('timer'),
                clearTimeout: () => undefined,
            },
        })
        coordinator.initialize(1)
        database.username = 'Published revision'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('ordinary-save')

        database.username = 'Completed generation'
        coordinator.markPersistentDataDirty(1)
        const result = await Promise.race([
            coordinator.flushPendingDataLocally('generation-completion').then(() => 'committed'),
            new Promise<string>((resolve) => setTimeout(() => resolve('blocked'), 25)),
        ])

        expect(result).toBe('committed')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(dispose).toHaveBeenCalledOnce()
    })

    it('serializes publication completion with an in-flight local generation commit', async () => {
        const database = makeDatabase()
        const publication = deferred<void>()
        const generationCommit = deferred<{ revision: number }>()
        const publish = vi.fn(() => publication.promise)
        const commit = vi.fn()
            .mockResolvedValueOnce({ revision: 2 })
            .mockImplementationOnce(() => generationCommit.promise)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) })),
            },
        })
        coordinator.initialize(1)
        database.username = 'First revision'
        coordinator.markPersistentDataDirty(1)
        const ordinaryFlush = coordinator.flushPendingData('ordinary-save')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())

        database.username = 'Completed generation'
        coordinator.markPersistentDataDirty(1)
        const localFlush = coordinator.flushPendingDataLocally('generation-completion')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledTimes(2))
        publication.resolve()
        const ordinaryState = await Promise.race([
            ordinaryFlush.then(() => 'settled', () => 'rejected'),
            new Promise<string>((resolve) => setTimeout(() => resolve('pending'), 25)),
        ])

        expect(ordinaryState).toBe('pending')
        expect(commit).toHaveBeenCalledTimes(2)
        generationCommit.resolve({ revision: 3 })
        await localFlush
        await ordinaryFlush

        expect(coordinator.revision).toBe(3)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
    })

    it('rejects local generation acknowledgement when the PDS commit fails', async () => {
        const database = makeDatabase()
        const error = new Error('local PDS failed')
        const commit = vi.fn().mockRejectedValue(error)
        const pin = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Uncommitted generation'
        coordinator.markPersistentDataDirty(25)
        await expect(
            coordinator.flushPendingDataLocally('generation-completion'),
        ).rejects.toBe(error)

        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(25)
        expect(coordinator.hasPendingOfficialPublication).toBe(false)
        expect(pin).not.toHaveBeenCalled()
    })

    it('autonomously retries a failed official publication while preserving its lease', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn()
                .mockRejectedValueOnce(new Error('offline'))
                .mockResolvedValueOnce(undefined)
            const handle = {
                publish,
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn(async () => handle)
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'Autonomous retry'
            coordinator.markPersistentDataDirty(1)

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledOnce()
            expect(pin).toHaveBeenCalledOnce()
            expect(publish).toHaveBeenCalledOnce()
            expect(handle.dispose).not.toHaveBeenCalled()
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await vi.advanceTimersByTimeAsync(2_999)
            expect(publish).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(1)

            expect(pin).toHaveBeenCalledOnce()
            expect(publish).toHaveBeenCalledTimes(2)
            expect(handle.dispose).toHaveBeenCalledOnce()
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('supersedes a failed publication with the newer local revision', async () => {
        const database = makeDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('remote')).mockResolvedValueOnce(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn().mockResolvedValue(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        database.username = 'Local one'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('first')).rejects.toThrow('remote')
        database.username = 'Local two'
        coordinator.markPersistentDataDirty(1)

        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(publish).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenNthCalledWith(1, 2)
        expect(pin).toHaveBeenNthCalledWith(2, 3)
        expect(handle.dispose).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].expectedRevision).toBe(2)
    })

    it('retries cleanup of a superseded publication without blocking the newer revision', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let nowValue = 0
            const staleHandle = {
                publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
                dispose: vi.fn()
                    .mockRejectedValueOnce(new Error('cleanup failed'))
                    .mockResolvedValueOnce(undefined),
            }
            const currentHandle = {
                publish: vi.fn(async () => undefined),
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn()
                .mockResolvedValueOnce(staleHandle)
                .mockResolvedValueOnce(currentHandle)
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
                now: () => nowValue,
                onBackgroundError,
            })
            coordinator.initialize(1)
            database.username = 'First revision'
            coordinator.markPersistentDataDirty(1)
            await expect(coordinator.flushPendingData('first')).rejects.toThrow('remote failed')

            nowValue = 4_000
            database.username = 'Newer revision'
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingData('newer')

            expect(pin).toHaveBeenNthCalledWith(2, 3)
            expect(currentHandle.publish).toHaveBeenCalledOnce()
            expect(currentHandle.dispose).toHaveBeenCalledOnce()
            expect(staleHandle.dispose).toHaveBeenCalledOnce()
            expect(onBackgroundError).toHaveBeenCalledWith(expect.objectContaining({
                message: 'cleanup failed',
            }))

            await vi.advanceTimersByTimeAsync(3_000)

            expect(staleHandle.dispose).toHaveBeenCalledTimes(2)
            expect(currentHandle.publish).toHaveBeenCalledOnce()
        } finally {
            vi.useRealTimers()
        }
    })

    it('spaces official publishes at least three seconds apart and publishes the newest revision', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(pin).toHaveBeenCalledWith(2)

            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Third edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledTimes(3)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await vi.advanceTimersByTimeAsync(2000)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(4)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('adopts a validated materialized database without dropping a throttled official publication', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            expect(coordinator.adoptMaterializedDatabase(
                coordinator.revision,
                coordinator.mutationGeneration,
                structuredClone(database),
            )).toBe(true)

            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            await vi.advanceTimersByTimeAsync(2500)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(3)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('adopts materialized baselines without traversing inactive conversation bodies', async () => {
        let database = makeDatabase()
        database.pluginCustomStorage = {}
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () =>
                database.characters.find((candidate) => candidate.chaId === 'char-a') ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7)

        const authoritative = makeDatabase()
        authoritative.username = 'Adopted root'
        authoritative.botPresets = [
            { name: 'Adopted preset', mainPrompt: 'Before preset edit' },
        ] as Database['botPresets']
        authoritative.pluginCustomStorage = JSON.parse(
            '{"zeta":0,"__proto__":false,"alpha":""}',
        )
        authoritative.characters[0].chats = [{
            id: 'chat-a',
            name: 'Selected chat',
            note: '',
            localLore: [],
            message: [{ chatId: 'message-a', role: 'char', data: 'Before message edit' }],
        } as Chat]
        const inactive = structuredClone(authoritative.characters[0])
        inactive.chaId = 'char-b'
        inactive.name = 'Inactive'
        inactive.chats = [{
            id: 'chat-b',
            name: 'Inactive chat',
            note: '',
            localLore: [],
            message: [{ chatId: 'message-b', role: 'char', data: 'Do not traverse' }],
        } as Chat]
        Object.defineProperty(inactive.chats[0].message[0], 'data', {
            enumerable: true,
            get: () => {
                throw new Error('inactive conversation body was traversed')
            },
        })
        authoritative.characters.push(inactive)

        expect(coordinator.adoptMaterializedDatabase(
            7,
            coordinator.mutationGeneration,
            authoritative,
        )).toBe(true)
        database = authoritative

        await coordinator.flushPendingData('clean-adopted-baseline')
        expect(commit).not.toHaveBeenCalled()
        expect(Object.keys(database.pluginCustomStorage)).toEqual([
            'zeta',
            '__proto__',
            'alpha',
        ])
        expect(Object.hasOwn(database.pluginCustomStorage, '__proto__')).toBe(true)
        expect(database.pluginCustomStorage.__proto__).toBe(false)
        expect(database.pluginCustomStorage).toMatchObject({
            zeta: 0,
            alpha: '',
        })

        database.username = 'Edited root'
        database.botPresets[0].mainPrompt = 'After preset edit'
        database.pluginCustomStorage.__proto__ = 0
        database.characters[0].chats[0].message[0].data = 'After message edit'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('after-adopted-mutations')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            root: { username: 'Edited root' },
            pluginStorage: [{ type: 'set', key: '__proto__', value: 0 }],
            replacePresets: [
                { name: 'Adopted preset', mainPrompt: 'After preset edit' },
            ],
            conversations: [{
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'chat-a',
                start: 0,
                deleteCount: 1,
                messages: [{
                    chatId: 'message-a',
                    role: 'char',
                    data: 'After message edit',
                }],
                conversation: {
                    id: 'chat-a',
                    name: 'Selected chat',
                    note: '',
                    localLore: [],
                },
            }],
        })
    })

    it('publishes immediately on explicit request despite the publish interval', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await coordinator.publishCurrentOfficialRevision()

            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(3)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
            await vi.advanceTimersByTimeAsync(4000)
            expect(pin).toHaveBeenCalledTimes(2)
        } finally {
            vi.useRealTimers()
        }
    })

    it('drops a deferred publication when an authoritative replacement supersedes it', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const store = {
                commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 9 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')

            expect(coordinator.hasPendingOfficialPublication).toBe(false)
            await vi.advanceTimersByTimeAsync(4000)
            expect(pin).toHaveBeenCalledTimes(1)
        } finally {
            vi.useRealTimers()
        }
    })

    it('retargets a throttled publication to a local replacement revision', async () => {
        vi.useFakeTimers()
        try {
            let database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const store = {
                commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 9 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => {
                    database = replacement
                },
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            const replacement = makeDatabase()
            replacement.username = 'Local compatibility replacement'
            await coordinator.replacePreparedPersistentDatabase(
                async () => replacement,
                'plugin-profile-change',
                { publishOfficial: true },
            )

            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            await vi.advanceTimersByTimeAsync(2500)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(9)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('exposes whether an official publication is still pending', async () => {
        const database = makeDatabase()
        let offline = true
        let nowValue = 0
        const publish = vi.fn(async () => {
            if (offline) throw new Error('offline')
        })
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        expect(coordinator.hasPendingOfficialPublication).toBe(false)

        database.username = 'Offline edit'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('offline')).rejects.toThrow('offline')
        expect(coordinator.hasPendingOfficialPublication).toBe(true)

        offline = false
        nowValue = 4000
        await coordinator.flushPendingData('online')
        expect(coordinator.hasPendingOfficialPublication).toBe(false)
    })

    it('reports repeated background publish failures once until a flush succeeds', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let offline = true
            const publish = vi.fn(async () => {
                if (offline) throw new Error('offline')
            })
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
                onBackgroundError,
            })
            coordinator.initialize(1)

            database.username = 'Edit one'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(onBackgroundError).toHaveBeenCalledTimes(1)

            database.username = 'Edit two'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(onBackgroundError).toHaveBeenCalledTimes(1)

            offline = false
            database.username = 'Edit three'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            await vi.advanceTimersByTimeAsync(2000)

            offline = true
            database.username = 'Edit four'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            await vi.advanceTimersByTimeAsync(2500)
            expect(onBackgroundError).toHaveBeenCalledTimes(2)
            expect(commit).toHaveBeenCalledTimes(4)
        } finally {
            vi.useRealTimers()
        }
    })

    it('retries pinning the committed revision before creating a newer local revision', async () => {
        const database = makeDatabase()
        const handle = {
            publish: vi.fn(async () => undefined),
            dispose: vi.fn(async () => undefined),
        }
        const pin = vi.fn().mockRejectedValueOnce(new Error('pin failed')).mockResolvedValueOnce(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Committed locally'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.flushPendingData('first')).rejects.toThrow('pin failed')
        await coordinator.flushPendingData('retry')

        expect(pin).toHaveBeenNthCalledWith(1, 2)
        expect(pin).toHaveBeenNthCalledWith(2, 2)
        expect(commit).toHaveBeenCalledTimes(1)
        expect(handle.publish).toHaveBeenCalledTimes(1)
        expect(handle.dispose).toHaveBeenCalledTimes(1)
    })

    it('disposes a failed pre-replacement publication so it can never publish later', async () => {
        const database = makeDatabase()
        const staleHandle = {
            publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
            dispose: vi.fn(async () => undefined),
        }
        const pin = vi.fn(async () => staleHandle)
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Local revision'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('publish')).rejects.toThrow('remote failed')

        const replacement = makeDatabase()
        replacement.username = 'Authoritative replacement'
        await coordinator.replacePersistentDatabase(replacement, 'replace')
        await coordinator.flushPendingData('clean')

        expect(staleHandle.dispose).toHaveBeenCalledTimes(1)
        expect(staleHandle.publish).toHaveBeenCalledTimes(1)
        expect(pin).toHaveBeenCalledTimes(1)
    })

    it('retries failed stale-publication cleanup after an authoritative replacement', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const staleHandle = {
                publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
                dispose: vi.fn()
                    .mockRejectedValueOnce(new Error('cleanup failed'))
                    .mockResolvedValueOnce(undefined),
            }
            const pin = vi.fn(async () => staleHandle)
            const store = {
                commit: vi.fn(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => Object.assign(database, replacement),
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'Local revision'
            coordinator.markPersistentDataDirty(1)
            await expect(coordinator.flushPendingData('publish')).rejects.toThrow('remote failed')

            await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')

            expect(staleHandle.dispose).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(3_000)
            expect(staleHandle.dispose).toHaveBeenCalledTimes(2)
            expect(staleHandle.publish).toHaveBeenCalledOnce()
            expect(pin).toHaveBeenCalledOnce()
        } finally {
            vi.useRealTimers()
        }
    })

    it('serializes replacement and leaves post-capture mutations for the next ordinary flush', async () => {
        const database = makeDatabase()
        const firstCommit = deferred<{ revision: number }>()
        const replacementWrite = deferred<{ revision: number }>()
        const commit = vi
            .fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockResolvedValueOnce({ revision: 4 })
        const replaceFromDatabase = vi.fn(() => replacementWrite.promise)
        const store = {
            commit,
            replaceFromDatabase,
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, replacement)
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(1)
        database.username = 'Commit before replacement'
        coordinator.markPersistentDataDirty(1)
        const flushing = coordinator.flushPendingData('active')
        const candidate = makeDatabase()
        candidate.username = 'Replacement'
        const replacing = coordinator.replacePersistentDatabase(candidate, 'replace')

        expect(replaceFromDatabase).not.toHaveBeenCalled()
        firstCommit.resolve({ revision: 2 })
        await flushing
        await vi.waitFor(() => expect(replaceFromDatabase).toHaveBeenCalledTimes(1))
        expect(replaceFromDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ username: 'Replacement' }),
            2,
        )
        expect(replaceDatabase).not.toHaveBeenCalled()
        database.characters[0].name = 'Mutation after capture'
        coordinator.markPersistentDataDirty(12)
        replacementWrite.resolve({ revision: 3 })

        await replacing

        expect(commit).toHaveBeenCalledTimes(1)
        expect(replaceDatabase).toHaveBeenCalledTimes(1)
        expect(replaceDatabase.mock.calls[0][0].characters[0].name).toBe(
            'Mutation after capture',
        )
        expect(coordinator.revision).toBe(3)
        expect(coordinator.pendingBytes).toBe(12)

        await coordinator.flushPendingData('post-replacement')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 3,
            replaceCharacter: { name: 'Mutation after capture' },
        })
        expect(coordinator.revision).toBe(4)
    })

    it('three-way rebases only concurrent live deltas onto an authoritative replacement', async () => {
        const database = makeDatabase()
        database.mainPrompt = 'Before prompt'
        database.botPresets = [
            { name: 'Before first', mainPrompt: 'first' },
            { name: 'Before second', mainPrompt: 'second' },
        ] as Database['botPresets']
        ;(database.characters[0] as character).desc = 'Before description'
        database.characters[0].chats = [
            { id: 'chat-a', name: 'First chat', note: '', localLore: [], message: [] },
            { id: 'chat-b', name: 'Second chat', note: '', localLore: [], message: [] },
        ]
        const candidate = structuredClone(database)
        candidate.username = 'Authoritative username'
        candidate.botPresets[1].mainPrompt = 'Authoritative second prompt'
        candidate.characters[0].name = 'Authoritative character name'
        candidate.characters[0].chats = [
            { ...candidate.characters[0].chats[1], name: 'Authoritative second chat' },
            candidate.characters[0].chats[0],
        ]
        const replacementWrite = deferred<{ revision: number }>()
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, structuredClone(replacement))
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(5)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'authoritative', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.mainPrompt = 'Later live prompt'
        database.botPresets[0].name = 'Later live first'
        ;(database.characters[0] as character).desc = 'Later live description'
        database.characters[0].chats[0].note = 'Later live first-chat note'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 6 })
        await replacing

        expect(database).toMatchObject({
            username: 'Authoritative username',
            mainPrompt: 'Later live prompt',
        })
        expect(database.botPresets).toMatchObject([
            { name: 'Later live first', mainPrompt: 'first' },
            { name: 'Before second', mainPrompt: 'Authoritative second prompt' },
        ])
        expect(database.characters[0]).toMatchObject({
            name: 'Authoritative character name',
            desc: 'Later live description',
        })
        expect(database.characters[0].chats).toMatchObject([
            { id: 'chat-b', name: 'Authoritative second chat' },
            { id: 'chat-a', note: 'Later live first-chat note' },
        ])
    })

    it('rebases a concurrent compatibility plugin edit and persists it after replacement', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = JSON.parse(
            '{"retained":"before","__proto__":"before"}',
        )
        const storagePrototype = Object.getPrototypeOf(database.pluginCustomStorage)
        const candidate = structuredClone(database)
        candidate.pluginCustomStorage.retained = 'replacement'
        const replacementWrite = deferred<{ revision: number }>()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit,
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
        })
        coordinator.initialize(5, database)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'plugin-rebase', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.pluginCustomStorage.__proto__ = 'later'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 6 })

        await replacing

        expect(Object.keys(database.pluginCustomStorage)).toEqual(['retained', '__proto__'])
        expect(database.pluginCustomStorage.retained).toBe('replacement')
        expect(Object.hasOwn(database.pluginCustomStorage, '__proto__')).toBe(true)
        expect(database.pluginCustomStorage.__proto__).toBe('later')
        expect(Object.getPrototypeOf(database.pluginCustomStorage)).toBe(storagePrototype)
        expect(commit).not.toHaveBeenCalled()

        await coordinator.flushPendingData('plugin-rebase-save')

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 6,
            pluginStorage: [{ type: 'set', key: '__proto__', value: 'later' }],
        })
    })

    it('keeps preset entities intact across an authoritative reorder and concurrent rename', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
        })
        coordinator.initialize(6)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'preset-reorder', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.botPresets[0].name = 'Preset A2'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 7 })
        await replacing

        expect(database.botPresets).toMatchObject([
            { name: 'Preset B', mainPrompt: 'Candidate B' },
            { name: 'Preset A2', mainPrompt: 'A' },
        ])
    })

    it('rejects an ambiguous preset reorder and rename before replacing the store', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        candidate.botPresets[1].name = 'Preset A2'
        const store = {
            replaceFromDatabase: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'ambiguous-rename', {
            authoritative: true,
        })
        database.botPresets[0].mainPrompt = 'Later A'
        coordinator.markPersistentDataDirty(1)

        await expect(replacing).rejects.toThrow('renamed candidate array entries')
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(database.botPresets).toMatchObject([
            { name: 'Preset A', mainPrompt: 'Later A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ])
    })

    it('rejects an all-renamed preset reorder before live edits can attach to another entity', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets = [
            { ...candidate.botPresets[1], name: 'Preset B2' },
            { ...candidate.botPresets[0], name: 'Preset A2' },
        ]
        const store = {
            replaceFromDatabase: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'all-renamed-reorder', {
            authoritative: true,
        })
        database.botPresets[0].temperature = 1.3
        coordinator.markPersistentDataDirty(1)

        await expect(replacing).rejects.toThrow('renamed candidate array entries')
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(database.botPresets).toMatchObject([
            { name: 'Preset A', mainPrompt: 'A', temperature: 1.3 },
            { name: 'Preset B', mainPrompt: 'B' },
        ])
    })

    it.each(['add', 'delete'] as const)(
        'preserves an authoritative preset %s and a concurrent live edit',
        async (operation) => {
            const database = makeDatabase()
            database.botPresets = [
                { name: 'Preset A', mainPrompt: 'A' },
                { name: 'Preset B', mainPrompt: 'B' },
            ] as Database['botPresets']
            const candidate = structuredClone(database)
            if (operation === 'add') {
                candidate.botPresets.splice(1, 0, {
                    name: 'Imported preset',
                    mainPrompt: 'Imported',
                } as Database['botPresets'][number])
            } else {
                candidate.botPresets.splice(0, 1)
            }
            const replacementWrite = deferred<{ revision: number }>()
            const store = {
                replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => Object.assign(database, replacement),
            })
            coordinator.initialize(7)

            const replacing = coordinator.replacePersistentDatabase(
                candidate,
                `preset-${operation}`,
                { authoritative: true },
            )
            await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
            database.botPresets[1].mainPrompt = 'Later B'
            coordinator.markPersistentDataDirty(1)
            replacementWrite.resolve({ revision: 8 })
            await replacing

            expect(database.botPresets).toMatchObject(operation === 'add'
                ? [
                    { name: 'Preset A', mainPrompt: 'A' },
                    { name: 'Imported preset', mainPrompt: 'Imported' },
                    { name: 'Preset B', mainPrompt: 'Later B' },
                ]
                : [{ name: 'Preset B', mainPrompt: 'Later B' }])
        },
    )

    it('rejects an ambiguous id-less preset reorder instead of publishing a hybrid', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Duplicate', mainPrompt: 'A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, structuredClone(replacement))
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(8)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'ambiguous-presets', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.botPresets[0].mainPrompt = 'Later A'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 9 })

        await expect(replacing).rejects.toThrow('Concurrent live changes conflicted')
        expect(replaceDatabase).toHaveBeenCalledOnce()
        expect(database.botPresets).toMatchObject([
            { name: 'Duplicate', mainPrompt: 'Later A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ])
        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 9,
            replacePresets: database.botPresets,
        })
        expect(coordinator.revision).toBe(10)

        await coordinator.flushPendingData('after-conflict')
        expect(store.commit).toHaveBeenCalledOnce()
    })

    it('does not baseline a live edit made while replacement compensation is pending', async () => {
        let database = makeDatabase()
        database.botPresets = [
            { name: 'Duplicate', mainPrompt: 'A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const compensationWrite = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => compensationWrite.promise)
            .mockResolvedValueOnce({ revision: 11 })
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit,
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
        })
        coordinator.initialize(8)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'pending-compensation', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.botPresets[0].mainPrompt = 'Later A'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 9 })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.botPresets[0].mainPrompt = 'Newest A'
        coordinator.markPersistentDataDirty(1)
        compensationWrite.resolve({ revision: 10 })

        await expect(replacing).rejects.toThrow('Concurrent live changes conflicted')
        expect(commit.mock.calls[0][0].replacePresets[0].mainPrompt).toBe('Later A')

        await coordinator.flushPendingData('after-pending-compensation')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 10,
            replacePresets: [
                { name: 'Duplicate', mainPrompt: 'Newest A' },
                { name: 'Duplicate', mainPrompt: 'B' },
            ],
        })
    })

    it('re-arms the flush debounce when a replacement leaves dirty state behind', async () => {
        vi.useFakeTimers()
        try {
            let db = makeDatabase()
            const gate = deferred<{ revision: number }>()
            const commit = vi.fn()
                .mockImplementationOnce(() => gate.promise)
                .mockImplementation(async ({ expectedRevision }: { expectedRevision: number }) => ({
                    revision: expectedRevision + 1,
                }))
            const store = {
                commit,
                replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(db),
                captureSelectedCharacter: () => db.characters[0],
                replaceDatabase: (replacement) => {
                    db = replacement
                },
            })
            coordinator.initialize(1)
            db.username = 'Edit one'
            coordinator.markPersistentDataDirty(1)
            const flushing = coordinator.flushPendingData('first')
            await vi.advanceTimersByTimeAsync(0)
            const candidate = makeDatabase()
            candidate.username = 'Replacement'
            const replacing = coordinator.replacePersistentDatabase(candidate, 'replace')
            gate.resolve({ revision: 2 })
            await Promise.resolve()
            db.username = 'Edit two'
            coordinator.markPersistentDataDirty(1)
            await flushing
            await replacing

            expect(commit).toHaveBeenCalledTimes(1)
            expect(db.username).toBe('Edit two')

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toMatchObject({
                expectedRevision: 3,
                root: { username: 'Edit two' },
            })
        } finally {
            vi.useRealTimers()
        }
    })

    it('finishes flushing after the selected character is deselected', async () => {
        const database = makeDatabase()
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        selectedIndex = -1
        database.username = 'Deselected'
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('deselected')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({ root: { username: 'Deselected' } })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(coordinator.pendingBytes).toBe(0)
    })

    it('rejects hydrated character adoption after the mutation generation changes', () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(4)
        const mutationGeneration = coordinator.mutationGeneration

        coordinator.markPersistentDataDirty(1)

        expect(coordinator.adoptHydratedCharacter(
            4,
            mutationGeneration,
            database.characters[0],
        )).toBe(false)
    })

    it('commits the newly selected character after the selection changes', async () => {
        const database = makeDatabase()
        const second = structuredClone(database.characters[0])
        second.chaId = 'char-b'
        second.name = 'Beta'
        database.characters.push(second)
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        selectedIndex = 1
        database.characters[1].name = 'Beta edited'
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('reselected')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({
            replaceCharacter: { chaId: 'char-b', name: 'Beta edited' },
        })
    })

    it('rescues the previous character edits when the selection switches directly', async () => {
        const database = makeDatabase()
        const second = structuredClone(database.characters[0])
        second.chaId = 'char-b'
        second.name = 'Beta'
        database.characters.push(second)
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        database.characters[0].name = 'Alpha edited'
        selectedIndex = 1
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('switched')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Alpha edited',
        })
        expect(commit.mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-b',
            name: 'Beta',
        })

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('does not publish or change baselines when replacement fails', async () => {
        const database = makeDatabase()
        const replaceDatabase = vi.fn()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = {
            commit,
            replaceFromDatabase: vi.fn().mockRejectedValue(new Error('replacement failed')),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(7)
        database.username = 'Still dirty'
        coordinator.markPersistentDataDirty(9)

        await expect(
            coordinator.replacePersistentDatabase(makeDatabase(), 'replace'),
        ).rejects.toThrow('replacement failed')

        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(7)
        expect(coordinator.pendingBytes).toBe(9)
        await coordinator.flushPendingData('after-failure')
        expect(commit).toHaveBeenCalledWith(
            expect.objectContaining({ expectedRevision: 7, root: { username: 'Still dirty' } }),
        )
    })

    describe('windowed selected-conversation persistence authority', () => {
        type TestWindowedAuthority = Omit<
            WindowedConversationPersistenceAuthority,
            'sessionToken'
        > & {
            sessionToken: any
        }

        function makeWindowedProjection() {
            const projectedConversation: Record<string, unknown> = {
                id: 'two',
                name: 'Two',
                localLore: [],
                note: '',
            }
            Object.defineProperty(projectedConversation, 'message', {
                enumerable: true,
                get: () => {
                    throw new Error('projected messages must not be traversed')
                },
            })
            return {
                character: {
                    type: 'character',
                    chaId: 'char-a',
                    name: 'Alpha',
                    chats: [projectedConversation],
                } as unknown as character,
                projectedConversation,
            }
        }

        function makeWindowedHarness(options: {
            totalMessages?: number
            readRevision?: number
            commit?: Parameters<typeof makeStore>[0]
        } = {}) {
            const database = makeDatabase()
            const { character: projection, projectedConversation } = makeWindowedProjection()
            let selected: character = projection
            const sessionToken = 'windowed-session' as any
            let authority: TestWindowedAuthority | null = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken,
                storeRevision: 2,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: options.totalMessages ?? 10_000,
            }
            const commit = options.commit ?? vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const readConversationWindow = vi.fn(async () => ({
                revision: options.readRevision ?? 2,
                value: {
                    characterId: 'char-a',
                    conversationId: 'two',
                    messages: [{ role: 'user', data: 'first persisted message' }],
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: options.totalMessages ?? 10_000,
                    hasMoreBefore: false,
                    hasMoreAfter: true,
                },
            }))
            const store = {
                ...makeStore(commit),
                readConversationWindow,
            } as unknown as PersistentDataStore
            const onPersisted = vi.fn((event) => {
                if (authority?.sessionToken !== event.sessionToken) return
                authority = {
                    ...authority,
                    storeRevision: event.revision,
                    persistedSessionVersion: event.sessionVersion,
                }
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                replaceDatabase: () => undefined,
                onConversationMutationPersisted: onPersisted,
            })
            coordinator.initialize(2, database)
            expect(coordinator.adoptWindowedSelectedConversation(
                2,
                coordinator.mutationGeneration,
                projection,
                authority,
            )).toBe(true)
            return {
                authority: () => authority,
                commit,
                coordinator,
                database,
                onPersisted,
                projectedConversation,
                readConversationWindow,
                replaceAuthority: (next: TestWindowedAuthority | null) => {
                    authority = next
                },
                replaceSelected: (next: character) => {
                    selected = next
                },
                sessionToken,
            }
        }

        function recordWindowedMutation(
            coordinator: SaveCoordinator,
            options: {
                characterId?: string
                conversationId?: string
                sessionToken: any
                previousVersion: number
                sessionVersion: number
                start: number
                deleteCount: number
                messages: Array<{ role: string; data: string }>
                conversationName?: string
            },
        ) {
            coordinator.recordActiveConversationMutation({
                characterId: options.characterId ?? 'char-a',
                conversationId: options.conversationId ?? 'two',
                sessionToken: options.sessionToken,
                previousVersion: options.previousVersion,
                sessionVersion: options.sessionVersion,
                commands: ['replace-range'],
                mutations: [{
                    start: options.start,
                    deleteCount: options.deleteCount,
                    messages: options.messages as any,
                    sessionVersion: options.sessionVersion,
                }],
                conversation: {
                    id: options.conversationId ?? 'two',
                    name: options.conversationName ?? 'Two',
                    localLore: [],
                    note: '',
                },
            })
        }

        it('rejects windowed adoption while ordinary dirty work is pending', () => {
            const database = makeDatabase()
            const { character: projection } = makeWindowedProjection()
            const sessionToken = 'windowed-session' as any
            let selected = database.characters[0]
            let authority: TestWindowedAuthority | null = null
            const coordinator = new SaveCoordinator({
                store: makeStore(),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(2, database)
            database.username = 'unsaved root change'
            coordinator.markPersistentDataDirty(1)
            selected = projection
            authority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken,
                storeRevision: 2,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }

            expect(coordinator.adoptWindowedSelectedConversation(
                2,
                coordinator.mutationGeneration,
                projection,
                authority,
            )).toBe(false)
        })

        it('commits absolute ranges from a 10k windowed projection without traversing projected messages', async () => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'bounded edit' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })

            await harness.coordinator.flushPendingData('windowed-absolute-range')

            expect(harness.readConversationWindow).toHaveBeenCalledWith({
                characterId: 'char-a',
                conversationId: 'two',
                startIndex: 0,
                limit: 1,
            })
            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.commit.mock.calls[0][0]).toMatchObject({
                expectedRevision: 2,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'two',
                    start: 9_500,
                    deleteCount: 1,
                    messages: [{ role: 'char', data: 'bounded edit' }],
                }],
            })
            expect(harness.commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
            expect(harness.onPersisted).toHaveBeenCalledWith(expect.objectContaining({
                sessionVersion: 1,
                revision: 3,
            }))
        })

        it.each([
            {
                label: 'out-of-range replacement',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionVersion: 1,
                }),
                event: { start: 10_001, deleteCount: 0, sessionVersion: 1 },
            },
            {
                label: 'final count mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionVersion: 1,
                    totalMessages: 10_001,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
            {
                label: 'session version mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionVersion: 2,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
            {
                label: 'session token mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionToken: 'other-session' as any,
                    sessionVersion: 1,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
            {
                label: 'authority revision mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    storeRevision: 1,
                    sessionVersion: 1,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
        ])('fails closed without replacement for $label', async ({ mutateAuthority, event }) => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: event.sessionVersion,
                start: event.start,
                deleteCount: event.deleteCount,
                messages: [{ role: 'char', data: 'invalid projection' }],
            })
            harness.replaceAuthority(mutateAuthority(harness.authority()!))

            await expect(
                harness.coordinator.flushPendingData(`windowed-${event.start}`),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit).not.toHaveBeenCalled()
            expect(harness.onPersisted).not.toHaveBeenCalled()
        })

        it('fails closed when the persistent range read returns another revision', async () => {
            const harness = makeWindowedHarness({ readRevision: 1 })
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'stale read' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-stale-read'),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit).not.toHaveBeenCalled()
            expect(harness.onPersisted).not.toHaveBeenCalled()
        })

        it('rejects complete-owner evidence instead of replacing all persisted messages', async () => {
            const harness = makeWindowedHarness()
            harness.coordinator.recordActiveConversationMutation({
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                commands: ['replace-conversation'],
                mutations: [{
                    start: 0,
                    deleteCount: 64,
                    messages: [{ role: 'char', data: 'partial projected owner' }],
                    sessionVersion: 1,
                    completeOwner: true,
                }],
                conversation: {
                    id: 'two',
                    name: 'Two',
                    localLore: [],
                    note: '',
                },
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
                totalMessages: 1,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-complete-owner'),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit).not.toHaveBeenCalled()
            expect(harness.onPersisted).not.toHaveBeenCalled()
        })

        it('checks current windowed authority before retrying resident compensation', async () => {
            const database = makeDatabase()
            const { character: projection } = makeWindowedProjection()
            let selected: character | groupChat = database.characters[0]
            let authority: TestWindowedAuthority | null = null
            let coordinator!: SaveCoordinator
            const commit = vi.fn(async ({ expectedRevision }) => {
                const call = commit.mock.calls.length
                if (call <= 4) {
                    ;(database.characters[0] as character).desc = `Concurrent edit ${call}`
                    coordinator.markPersistentDataDirty(1)
                }
                return { revision: expectedRevision + 1 }
            })
            const store = {
                ...makeStore(commit),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(10, database)
            await expect(coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'seed-pending-compensation',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )).rejects.toThrow('Resident character changed')
            expect(commit).toHaveBeenCalledTimes(4)

            selected = projection
            authority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken: 'windowed-after-compensation' as any,
                storeRevision: 14,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }

            await expect(
                coordinator.flushPendingData('windowed-before-compensation'),
            ).rejects.toThrow(/compatibility/i)
            expect(commit).toHaveBeenCalledTimes(4)
        })

        it('rechecks windowed authority after an awaited complete-character commit', async () => {
            const database = makeDatabase()
            const { character: projection } = makeWindowedProjection()
            let selected: character | groupChat = database.characters[0]
            let authority: TestWindowedAuthority | null = null
            const committed = deferred<{ revision: number }>()
            const commit = vi.fn(() => committed.promise)
            const store = {
                ...makeStore(commit),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(10, database)
            const replacing = coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'authority-switch-during-commit',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )
            await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

            selected = projection
            authority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken: 'windowed-during-commit' as any,
                storeRevision: 10,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }
            committed.resolve({ revision: 11 })

            await expect(replacing).rejects.toThrow(/compatibility/i)
            expect(commit).toHaveBeenCalledOnce()
        })

        it('acknowledges a contiguous multi-event prefix through one absolute-range commit', async () => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 100,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'inserted' }],
            })
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 1,
                sessionVersion: 2,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'edited' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 2,
                totalMessages: 10_001,
            })

            await harness.coordinator.flushPendingData('windowed-multi-event')

            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.commit.mock.calls[0][0].conversations).toHaveLength(2)
            expect(harness.onPersisted.mock.calls.map(([event]) => event.sessionVersion)).toEqual([
                1,
                2,
            ])
        })

        it('retains exact windowed evidence after failure and retries it unchanged', async () => {
            const commit = vi.fn()
                .mockRejectedValueOnce(new Error('windowed write failed'))
                .mockImplementation(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))
            const harness = makeWindowedHarness({ commit })
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'retry me' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-failure'),
            ).rejects.toThrow('windowed write failed')
            await harness.coordinator.flushPendingData('windowed-retry')

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0].conversations).toEqual(
                commit.mock.calls[0][0].conversations,
            )
            expect(harness.onPersisted).toHaveBeenCalledOnce()
        })

        it('retains windowed evidence after validation fails before the store commit', async () => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'retry validated evidence' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 2,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-invalid-version'),
            ).rejects.toThrow(/compatibility/i)
            expect(harness.commit).not.toHaveBeenCalled()

            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })
            await harness.coordinator.flushPendingData('windowed-valid-retry')

            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.onPersisted).toHaveBeenCalledOnce()
        })

        it.each([
            { label: 'shell mismatch', kind: 'shell' },
            { label: 'foreign conversation evidence', kind: 'foreign' },
        ])('never creates a character replacement for $label', async ({ kind }) => {
            const harness = makeWindowedHarness()
            if (kind === 'shell') {
                harness.projectedConversation.name = 'Unexplained metadata change'
                harness.coordinator.markPersistentDataDirty(1)
            } else {
                recordWindowedMutation(harness.coordinator, {
                    characterId: 'char-a',
                    conversationId: 'foreign',
                    sessionToken: harness.sessionToken,
                    previousVersion: 0,
                    sessionVersion: 1,
                    start: 0,
                    deleteCount: 0,
                    messages: [{ role: 'char', data: 'foreign' }],
                })
                harness.replaceAuthority({
                    ...harness.authority()!,
                    sessionVersion: 1,
                    totalMessages: 10_001,
                })
            }

            await expect(
                harness.coordinator.flushPendingData(`windowed-never-replace-${kind}`),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit.mock.calls.some(
                ([workingSet]) => workingSet.replaceCharacter !== undefined,
            )).toBe(false)
        })

        it('requires explicit complete adoption before restoring the complete fallback', async () => {
            const harness = makeWindowedHarness()
            const complete = {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chats: [{
                    id: 'two',
                    name: 'Two',
                    localLore: [],
                    note: '',
                    message: [{ role: 'user', data: 'complete body' }],
                }],
            } as unknown as character
            harness.replaceAuthority(null)
            harness.replaceSelected(complete)
            harness.coordinator.markPersistentDataDirty(1)

            await expect(
                harness.coordinator.flushPendingData('complete-without-adoption'),
            ).rejects.toThrow(/compatibility/i)
            expect(harness.commit).not.toHaveBeenCalled()

            expect(harness.coordinator.adoptHydratedCharacter(
                2,
                harness.coordinator.mutationGeneration,
                complete,
            )).toBe(true)
            complete.name = 'Complete fallback restored'
            harness.coordinator.markPersistentDataDirty(1)
            await harness.coordinator.flushPendingData('complete-after-adoption')

            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.commit.mock.calls[0][0].replaceCharacter).toMatchObject({
                chaId: 'char-a',
                name: 'Complete fallback restored',
            })
        })

        it('blocks persistence reentry during a selected-conversation authority transition', async () => {
            const harness = makeWindowedHarness()

            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
            expect(harness.coordinator.isSelectedConversationTransitionActive).toBe(false)
            expect(() => harness.coordinator.runSelectedConversationTransition(() => {
                expect(harness.coordinator.isSelectedConversationTransitionActive).toBe(true)
                harness.coordinator.markPersistentDataDirty(1)
            })).toThrow(/transition/i)
            expect(harness.coordinator.isSelectedConversationTransitionActive).toBe(false)
            expect(() => harness.coordinator.runSelectedConversationTransition(() =>
                harness.coordinator.flushPendingData('reentrant-transition'),
            )).toThrow(/transition/i)
            expect(harness.commit).not.toHaveBeenCalled()
        })

        it('advances an adopted windowed authority after a storage-only revision', async () => {
            const harness = makeWindowedHarness()

            await harness.coordinator.runStorageOnlyMutation(async () => 3)
            const advanced = {
                ...harness.authority()!,
                storeRevision: 3,
            }
            harness.replaceAuthority(advanced)

            expect(harness.coordinator.advanceWindowedSelectedConversationRevision(
                3,
                advanced,
            )).toBe(true)
            await harness.coordinator.flushPendingData('advanced-windowed-baseline')
            expect(harness.commit).not.toHaveBeenCalled()
        })
    })
})
