import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const runtime = vi.hoisted(() => ({
    flushPendingData: vi.fn(async (_reason: string) => undefined),
    readPreset: vi.fn(async (_id: string): Promise<{ revision: number, value: unknown } | null> => null),
    // Runs an operation where the runtime queues it behind every save and receive.
    pausedWrites: vi.fn(async (operation: () => Promise<unknown>) => operation()),
    mutatePresets: vi.fn(),
}))

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
vi.mock('./persistentDataRuntime.svelte', async (importOriginal) => ({
    ...await importOriginal<typeof import('./persistentDataRuntime.svelte')>(),
    flushPendingData: runtime.flushPendingData,
    mutatePersistentPresets: runtime.mutatePresets,
    getPersistentDataRuntime: () => ({ store: { readPreset: runtime.readPreset },
        withPausedPersistentWrites: (_reason: string, operation: () => Promise<unknown>) => runtime.pausedWrites(operation) }),
}))

import { selectedCharID } from '../stores.svelte'
import { doingChat } from '../process/generationState'
import type { Chat, Database, botPreset, character } from './database.svelte'
import {
    activatePresetOverride,
    activateRequestPresetBinding,
    applyConversationPresetBinding,
    captureCurrentPreset,
    carryPresetBinding,
    getDatabase,
    getEffectivePresetId,
    importPreset,
    normalizeDatabaseDefaults,
    setDatabase,
    setEffectivePresetOverride,
    type PresetSource,
} from './database.svelte'
import { derivePersistentIdentityMirrors } from './persistentIdentityHooks'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { getEffectivePresetOverride, isLoadedPreset } from './effectiveIdentityState'
import { createPresetCatalogWorkingSetFromValues } from './workingSetCatalog'
import { activatePresetChainForRequest } from '../process/presetChain'

function presetDatabase(): Database {
    const database = normalizeDatabaseDefaults({} as Database)
    database.botPresets[0].id = 'preset-a'
    database.botPresets[0].mainPrompt = 'Prompt A'
    database.botPresets.push({ ...structuredClone(database.botPresets[0]), id: 'preset-b', name: 'Preset B', mainPrompt: 'Prompt B' })
    database.botPresetsId = 0
    database.mainPrompt = 'Prompt A'
    database.personas[0].id = 'persona'
    return database
}

/** Holds preset B as a catalog summary whose body only the store keeps. */
function summarizePresetB(database: Database = getDatabase()): void {
    database.botPresets = createPresetCatalogWorkingSetFromValues(database.botPresets, 1, database.botPresets[database.botPresetsId].id)
    database.botPresets[1] = { id: 'preset-b', name: 'Preset B' } as botPreset
}

let stored: Map<string, botPreset>

function deferred() {
    let resolve!: () => void
    const promise = new Promise<void>((done) => { resolve = done })
    return { promise, resolve }
}

beforeEach(() => {
    const database = presetDatabase()
    stored = new Map(database.botPresets.map((preset) => [preset.id!, structuredClone(preset)]))
    setDatabase(database)
    selectedCharID.set(-1)
    runtime.flushPendingData.mockReset().mockResolvedValue(undefined)
    runtime.pausedWrites.mockReset().mockImplementation((operation) => operation())
    runtime.readPreset.mockReset().mockImplementation(async (id) => {
        const value = stored.get(id)
        return value ? { revision: 1, value: structuredClone(value) } : null
    })
})

describe('preset chain override activation', () => {
    it('activates the named preset when the library changes during the flush', async () => {
        summarizePresetB()
        const added = { ...structuredClone(stored.get('preset-a')!), id: 'preset-c', name: 'Preset C', mainPrompt: 'Prompt C' }
        stored.set(added.id, added)
        runtime.flushPendingData.mockImplementation(async () => {
            const database = getDatabase()
            database.botPresets.unshift(structuredClone(added))
            database.botPresetsId = 1
        })

        await activatePresetOverride('preset-b')

        expect(runtime.readPreset).toHaveBeenCalledExactlyOnceWith('preset-b')
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
    })

    it('activates the preset the chain of a library in another order names when it replaces this one first', async () => {
        getDatabase().presetChain = 'Preset A'

        await activatePresetChainForRequest(getDatabase, activatePresetOverride, () => 0, vi.fn(), async () => {
            const reordered = presetDatabase()
            reordered.botPresets.reverse()
            reordered.botPresetsId = 1
            reordered.presetChain = 'Preset B'
            setDatabase(reordered)
            return false
        })

        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
    })

    it('installs the stored value when the record is still a catalog stub', async () => {
        summarizePresetB()
        expect(isLoadedPreset(getDatabase(), 'preset-b')).toBe(false)

        await activatePresetOverride('preset-b')

        expect(getDatabase().botPresets[1]).toEqual(stored.get('preset-b'))
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
    })

    it('clears the override for a null index', async () => {
        setEffectivePresetOverride('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')

        await activatePresetOverride(null)

        expect(runtime.flushPendingData).toHaveBeenCalledOnce()
        expect(runtime.readPreset).not.toHaveBeenCalled()
        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')
    })

    it('holds the first preset whole when the selection names a removed one, even with no preset fields', () => {
        const database = getDatabase()
        database.botPresets[0] = { id: 'preset-a', name: 'Preset A' } as botPreset
        // A selection removed on another device falls back to the first preset.
        database.botPresets = createPresetCatalogWorkingSetFromValues(database.botPresets, 1, 'preset-removed')
        derivePersistentIdentityMirrors()

        expect(captureCurrentPreset()).toEqual({ id: 'preset-a', name: 'Preset A' })
        expect(runtime.readPreset).not.toHaveBeenCalled()
    })

    it('rejects a missing preset without changing the override', async () => {
        await expect(activatePresetOverride('preset-missing')).rejects.toThrow('Preset was not found')
        expect(runtime.readPreset).not.toHaveBeenCalled()

        const presetB = stored.get('preset-b')!
        stored.delete('preset-b')
        summarizePresetB()
        await expect(activatePresetOverride('preset-b')).rejects.toThrow('Preset was not found')

        stored.set('preset-b', presetB)
        runtime.flushPendingData.mockImplementation(async () => {
            getDatabase().botPresets.splice(1, 1)
        })
        await expect(activatePresetOverride('preset-b')).rejects.toThrow('Preset was not found')

        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')
    })
})

function conversation(id: string, promptPresetId: string): Chat {
    return { id, name: id, note: '', localLore: [], message: [], promptPresetId } as Chat
}

/** One character whose first conversation is selected. */
function selectConversations(...chats: Chat[]): Chat[] {
    const database = getDatabase()
    database.characters = [{ type: 'character', chaId: 'character', name: 'Character', chatPage: 0, chats } as unknown as character]
    selectedCharID.set(0)
    return database.characters[0].chats
}

function addPresetC(): void {
    const presetC = { ...structuredClone(stored.get('preset-a')!), id: 'preset-c', name: 'Preset C', mainPrompt: 'Prompt C' }
    stored.set(presetC.id, presetC)
    getDatabase().botPresets.push(structuredClone(presetC))
}

describe('conversation preset binding', () => {
    afterEach(() => doingChat.set(false))

    it('reads a bound catalog stub before applying it and drops the read another selection made stale', async () => {
        summarizePresetB()
        const read = deferred()
        const readStored = runtime.readPreset.getMockImplementation()!
        runtime.readPreset.mockImplementation(async (id) => {
            await read.promise
            return readStored(id)
        })
        const database = getDatabase()

        const stale = applyConversationPresetBinding(database, conversation('bound', 'preset-b'))
        await vi.waitFor(() => expect(runtime.readPreset).toHaveBeenCalledOnce())
        await applyConversationPresetBinding(database, conversation('unbound', ''))
        read.resolve()
        await stale

        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')

        await applyConversationPresetBinding(database, conversation('bound', 'preset-b'))
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
        expect(getDatabase().botPresetsId).toBe(0)
    })

    it.each(['before', 'after'] as const)('drops a stale binding whose preset a later selection read meanwhile (selection %s the body is placed)', async (when) => {
        summarizePresetB()
        const flush = deferred()
        runtime.pausedWrites.mockImplementationOnce(async (operation) => {
            if (when === 'before') await flush.promise
            const placed = await operation()
            if (when === 'after') await flush.promise
            return placed
        })
        const database = getDatabase()

        const stale = applyConversationPresetBinding(database, conversation('bound', 'preset-b'))
        if (when === 'before') await vi.waitFor(() => expect(runtime.pausedWrites).toHaveBeenCalledOnce())
        else await vi.waitFor(() => expect(isLoadedPreset(database, 'preset-b')).toBe(true))
        await applyConversationPresetBinding(database, conversation('bound', 'preset-b'))
        expect(getEffectivePresetId()).toBe('preset-b')
        await applyConversationPresetBinding(database, conversation('unbound', ''))
        flush.resolve()
        await stale

        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')
    })

    it('applies the binding of the conversation a request generates for, not the selected one', async () => {
        addPresetC()
        selectConversations(conversation('selected', 'preset-c'), conversation('target', 'preset-b'), conversation('plain', ''))
        await applyConversationPresetBinding(getDatabase(), getDatabase().characters[0].chats[0])
        setEffectivePresetOverride('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt C')

        expect(await activateRequestPresetBinding({ characterId: 'character', conversationId: 'target' })).toBe(true)
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getEffectivePresetOverride(getDatabase())).toBeNull()
        expect(getDatabase().mainPrompt).toBe('Prompt B')

        // An unbound target leaves the preset to the chain and the selection.
        expect(await activateRequestPresetBinding({ characterId: 'character', conversationId: 'plain' })).toBe(false)
        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')
    })

    it('follows the selected conversation after a received change, except while a request runs', async () => {
        const [selected] = selectConversations(conversation('selected', 'preset-b'))
        await applyConversationPresetBinding(getDatabase(), selected)
        getDatabase().botPresets[1].mainPrompt = 'Received B'
        derivePersistentIdentityMirrors()
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Received B')

        selected.promptPresetId = ''
        derivePersistentIdentityMirrors()
        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')

        await activateRequestPresetBinding({ characterId: 'character', conversationId: 'other' })
        doingChat.set(true)
        selected.promptPresetId = 'preset-b'
        derivePersistentIdentityMirrors()
        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')
    })

    it('drops a binding carried across a replacement when the selection applies one during the read', async () => {
        summarizePresetB()
        const read = deferred()
        runtime.readPreset.mockImplementationOnce(async (id) => {
            await read.promise
            return { revision: 1, value: structuredClone(stored.get(id)!) }
        })
        const carry = carryPresetBinding('preset-b', getPersistentDataRuntime() as unknown as PresetSource)
        await applyConversationPresetBinding(getDatabase(), conversation('unbound', ''))
        read.resolve()
        await carry

        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')
    })

    it.each([['chain', true], ['request', true], ['request', false]] as const)('reads the body into a library that replaced the one being read (%s, summary=%s)', async (path, summary) => {
        const owner = () => [{ type: 'character', chaId: 'character', name: 'Character', chatPage: 0,
            chats: [conversation('target', 'preset-b')] } as unknown as character]
        if (summary) summarizePresetB()
        getDatabase().characters = owner()
        const read = deferred()
        const previous = { revision: 1, value: structuredClone(stored.get('preset-b')!) }
        runtime.readPreset.mockImplementationOnce(async () => {
            await read.promise
            return previous
        })
        const activation = path === 'chain' ? activatePresetOverride('preset-b')
            : activateRequestPresetBinding({ characterId: 'character', conversationId: 'target' })
        // A held body needs no read, so the library is replaced before the activation resumes.
        if (summary) await vi.waitFor(() => expect(runtime.readPreset).toHaveBeenCalledOnce())
        stored.get('preset-b')!.mainPrompt = 'Replacement B'
        const replacement = presetDatabase()
        if (summary) summarizePresetB(replacement)
        else replacement.botPresets[1].mainPrompt = 'Replacement B'
        replacement.characters = owner()
        setDatabase(replacement)
        read.resolve()
        await activation

        expect(getDatabase().botPresets[1].mainPrompt).toBe('Replacement B')
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Replacement B')
    })

    it.each(['chain', 'request'] as const)('applies the preset again to a library that replaced this one right after the read (%s)', async (path) => {
        const owner = () => [{ type: 'character', chaId: 'character', name: 'Character', chatPage: 0,
            chats: [conversation('target', 'preset-b')] } as unknown as character]
        const replacement = presetDatabase()
        summarizePresetB(replacement)
        replacement.characters = owner()
        summarizePresetB()
        getDatabase().characters = owner()
        runtime.pausedWrites.mockImplementationOnce(async (operation) => {
            const result = await operation()
            // A replacement queued behind the read is installed before the activation resumes.
            setDatabase(replacement)
            return result
        })
        await (path === 'chain' ? activatePresetOverride('preset-b')
            : activateRequestPresetBinding({ characterId: 'character', conversationId: 'target' }))

        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
    })
})

describe('preset file import', () => {
    it('reports the index of the preset it added', async () => {
        runtime.mutatePresets.mockImplementation(async (_reason: string, mutate: (state: unknown) => void) => {
            const database = getDatabase()
            mutate({ root: database, presets: database.botPresets })
        })
        const data = new TextEncoder().encode(JSON.stringify({ name: 'Imported C', mainPrompt: 'Prompt C' }))
        await expect(importPreset({ name: 'synthetic.json', data })).resolves.toBe(2)
        expect(getDatabase().botPresets[2]).toMatchObject({ name: 'Imported C', mainPrompt: 'Prompt C' })
    })
})
