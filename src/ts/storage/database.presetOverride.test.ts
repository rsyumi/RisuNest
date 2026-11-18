import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const runtime = vi.hoisted(() => ({
    flushPendingData: vi.fn(async (_reason: string) => undefined),
    readPreset: vi.fn(async (_id: string): Promise<{ revision: number, value: unknown } | null> => null),
    mutatePresets: vi.fn(),
}))

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
vi.mock('./persistentDataRuntime.svelte', async (importOriginal) => ({
    ...await importOriginal<typeof import('./persistentDataRuntime.svelte')>(),
    flushPendingData: runtime.flushPendingData,
    mutatePersistentPresets: runtime.mutatePresets,
    getPersistentDataRuntime: () => ({ store: { readPreset: runtime.readPreset } }),
}))

import { selectedCharID } from '../stores.svelte'
import { doingChat } from '../process/generationState'
import type { Chat, Database, botPreset, character } from './database.svelte'
import {
    activatePresetOverride,
    activateRequestPresetBinding,
    applyConversationPresetBinding,
    carryPresetBinding,
    getDatabase,
    getEffectivePresetId,
    importPreset,
    normalizeDatabaseDefaults,
    setDatabase,
    setEffectivePresetOverride,
} from './database.svelte'
import { getEffectivePresetOverride } from './effectiveIdentityState'
import { derivePersistentIdentityMirrors } from './persistentIdentityHooks'
import { isLoadedPreset } from './persistentDataRuntime.svelte'

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
    runtime.readPreset.mockReset().mockImplementation(async (id) => {
        const value = stored.get(id)
        return value ? { revision: 1, value: structuredClone(value) } : null
    })
})

describe('preset chain override activation', () => {
    it('activates the preset the index named when the library changes during the flush', async () => {
        const added = { ...structuredClone(stored.get('preset-a')!), id: 'preset-c', name: 'Preset C', mainPrompt: 'Prompt C' }
        stored.set(added.id, added)
        runtime.flushPendingData.mockImplementation(async () => {
            const database = getDatabase()
            database.botPresets.unshift(structuredClone(added))
            database.botPresetsId = 1
        })

        await activatePresetOverride(1)

        expect(runtime.readPreset).toHaveBeenCalledExactlyOnceWith('preset-b')
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
    })

    it('keeps a loaded preset record that is edited while its stored value is read', async () => {
        stored.get('preset-b')!.mainPrompt = 'Stored B'
        const read = deferred()
        const readStored = runtime.readPreset.getMockImplementation()!
        runtime.readPreset.mockImplementation(async (id) => {
            await read.promise
            return readStored(id)
        })

        const activation = activatePresetOverride(1)
        await vi.waitFor(() => expect(runtime.readPreset).toHaveBeenCalledOnce())
        const record = getDatabase().botPresets[1]
        expect(isLoadedPreset(record)).toBe(true)
        record.mainPrompt = 'Edited B'
        read.resolve()
        await activation

        expect(getDatabase().botPresets[1]).toBe(record)
        expect(record.mainPrompt).toBe('Edited B')
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Edited B')
    })

    it('installs the stored value when the record is still a catalog stub', async () => {
        getDatabase().botPresets[1] = { id: 'preset-b', name: 'Preset B' } as botPreset
        expect(isLoadedPreset(getDatabase().botPresets[1])).toBe(false)

        await activatePresetOverride(1)

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

    it('rejects a missing preset without changing the override', async () => {
        await expect(activatePresetOverride(5)).rejects.toThrow('Preset was not found')
        expect(runtime.readPreset).not.toHaveBeenCalled()

        const presetB = stored.get('preset-b')!
        stored.delete('preset-b')
        await expect(activatePresetOverride(1)).rejects.toThrow('Preset was not found')

        stored.set('preset-b', presetB)
        runtime.flushPendingData.mockImplementation(async () => {
            getDatabase().botPresets.splice(1, 1)
        })
        await expect(activatePresetOverride(1)).rejects.toThrow('Preset was not found')

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
        getDatabase().botPresets[1] = { id: 'preset-b', name: 'Preset B' } as botPreset
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
        expect(runtime.flushPendingData).toHaveBeenCalled()
        expect(getEffectivePresetId()).toBe('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
        expect(getDatabase().botPresetsId).toBe(0)
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
        getDatabase().botPresets[1] = { id: 'preset-b', name: 'Preset B' } as botPreset
        const read = deferred()
        const carry = carryPresetBinding('preset-b', async (id) => {
            await read.promise
            return { value: structuredClone(stored.get(id)!) }
        })
        await applyConversationPresetBinding(getDatabase(), conversation('unbound', ''))
        read.resolve()
        await carry

        expect(getEffectivePresetId()).toBe('preset-a')
        expect(getDatabase().mainPrompt).toBe('Prompt A')
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
