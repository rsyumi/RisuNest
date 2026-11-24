import { readFileSync } from 'node:fs'
import ts from 'typescript'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { flushSync } from 'svelte'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
const installed = vi.hoisted(() => ({ runtime: undefined as PersistentDataRuntime | undefined }))
// The app's preset operations reach the runtime of the store each test builds.
vi.mock('../storage/persistentDataRuntime.svelte', async (importOriginal) => {
    const actual = await importOriginal<typeof import('../storage/persistentDataRuntime.svelte')>()
    const runtime = (): PersistentDataRuntime => {
        if (!installed.runtime) throw new Error('No persistent runtime installed')
        return installed.runtime
    }
    return {
        ...actual,
        getPersistentDataRuntime: runtime,
        flushPendingData: (reason) => runtime().flushPendingData(reason),
        mutatePersistentPresets: (reason, mutate) => runtime().mutatePersistentPresets(reason, mutate),
    } satisfies typeof actual
})

import { selectedCharID, settingsOpen } from '../stores.svelte'
import {
    applyConversationPresetBinding,
    carryPresetBinding,
    getDatabase,
    normalizeDatabaseDefaults,
    presetTemplate,
    setDatabaseLite,
    setPreset,
    type Database,
} from '../storage/database.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import { createPersistentDataRuntime } from '../storage/persistentDataRuntime'
import { createProductionStateAdapter } from '../storage/persistentDataRuntime.svelte'
import { preparePersistentRootForWorkingSet } from '../storage/databasePreparation'
import { presetEditorDBState } from '../storage/presetEditor.svelte'
import { projectCompleteScalableWorkingSet } from '../storage/workingSetCatalog'
import { workingSetResidency } from '../storage/workingSetResidency'
import type { PluginDatabaseAccess } from './pluginDatabaseAccess'
import { createRisunestPresetAccess, presetConnectionFields, presetDataFields } from './risunestPresetAccess'

const connectionFields = ['openAIKey', 'proxyKey', 'forceReplaceUrl', 'forceReplaceUrl2', 'koboldURL', 'textgenWebUIStreamURL', 'textgenWebUIBlockingURL']

function declaredPresetFields(): string[] {
    const path = 'src/ts/storage/database.svelte.ts'
    const source = ts.createSourceFile(path, readFileSync(path, 'utf8'), ts.ScriptTarget.Latest, true)
    const declaration = source.statements.find((statement): statement is ts.InterfaceDeclaration =>
        ts.isInterfaceDeclaration(statement) && statement.name.text === 'botPreset')
    if (!declaration) throw new Error('botPreset interface not found')
    expect(declaration.heritageClauses).toBeUndefined()
    return declaration.members.map((member) => {
        if (!ts.isPropertySignature(member) || !(ts.isIdentifier(member.name) || ts.isStringLiteral(member.name))) {
            throw new Error(`Unclassifiable botPreset member: ${member.getText(source)}`)
        }
        return member.name.text
    })
}

interface LibraryOptions {
    catalog?: boolean
    /** Binds the open conversation to this preset, which then applies instead of the selection. */
    bound?: string
    names?: string[]
    edit?(database: Database): void
}

async function presetLibrary({ catalog = false, bound, names = ['A', 'B', 'C'], edit }: LibraryOptions = {}) {
    const initial = normalizeDatabaseDefaults({} as Database)
    const template = initial.botPresets[0]
    initial.botPresets = names.map((name, index) => ({
        ...structuredClone(template), id: `preset-${name.toLowerCase()}`, name, mainPrompt: `Prompt ${name}`, temperature: 10 + index,
        openAIKey: `openai ${name}`, proxyKey: `proxy ${name}`, forceReplaceUrl: `https://${name}.invalid/1`,
        forceReplaceUrl2: `https://${name}.invalid/2`, koboldURL: `https://${name}.invalid/kobold`,
        textgenWebUIStreamURL: `wss://${name}.invalid`, textgenWebUIBlockingURL: `https://${name}.invalid/block`,
    }))
    initial.botPresetsId = 0
    initial.personas[0].id = 'persona'
    if (bound) {
        initial.characters = [{ type: 'character', chaId: 'character', name: 'Character', chatPage: 0, chatFolders: [],
            chats: [{ id: 'chat', name: 'Chat', note: '', localLore: [], message: [], promptPresetId: bound }] }] as unknown as Database['characters']
    }
    edit?.(initial)
    setPreset(initial, initial.botPresets[initial.botPresetsId])
    const store = new IndexedDbPersistentDataStore(`risunest-preset-access-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange)
    await store.open()
    const { revision } = await store.replaceFromDatabase(initial)
    const characterId = bound ? 'character' : null
    setDatabaseLite(catalog
        ? projectCompleteScalableWorkingSet(initial, characterId, revision, new Set(bound ? ['character'] : []))
        : structuredClone(initial))
    selectedCharID.set(bound ? 0 : -1)
    const runtime = createPersistentDataRuntime({ store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
        prepareRoot: (root) => preparePersistentRootForWorkingSet(root) })
    installed.runtime = runtime
    await runtime.initializeActiveWorkingSet(getDatabase())
    if (bound) await carryPresetBinding(bound, runtime)
    return {
        store,
        stored: async (id: string) => (await store.readPreset(id))?.value as unknown as Record<string, unknown> | undefined,
    }
}

function plugin({ read = true, write = true } = {}) {
    return createRisunestPresetAccess({
        pluginName: 'synthetic-plugin',
        databaseAccess: {} as PluginDatabaseAccess,
        hasDatabasePermission: async () => read,
        hasDatabaseWritePermission: async () => write,
        lifetimeSignal: new AbortController().signal,
    })
}

const summary = (items: { id: string; selected: boolean; applied: boolean }[]) => ({
    ids: items.map((item) => item.id),
    selected: items.filter((item) => item.selected).map((item) => item.id),
    applied: items.filter((item) => item.applied).map((item) => item.id),
})

afterEach(async () => {
    settingsOpen.set(false)
    flushSync()
    // A preset left bound would stay loaded in the next test's working set.
    if (installed.runtime) await applyConversationPresetBinding(getDatabase(), undefined)
    installed.runtime = undefined
    workingSetResidency.clear()
})

describe('preset field classification', () => {
    it('classifies every declared preset field once and names no other field', () => {
        const declared = declaredPresetFields()
        const classified = [...presetConnectionFields, ...presetDataFields]
        expect(new Set(classified).size).toBe(classified.length)
        expect(declared.filter((field) => !classified.includes(field))).toEqual([])
        expect(classified.filter((field) => !declared.includes(field))).toEqual([])
        expect([...presetConnectionFields].sort()).toEqual([...connectionFields].sort())
    })
})

describe('risunest preset methods', () => {
    it.each([false, true])('lists presets in app order with the selection and the preset the open conversation uses (catalog=%s)', async (catalog) => {
        await presetLibrary({ catalog, bound: 'preset-c', edit: (database) => { database.botPresetsId = 1 } })

        const listed = await plugin().listPresets()

        expect(listed.total).toBe(3)
        expect(listed.items.map((item) => item.name)).toEqual(['A', 'B', 'C'])
        expect(summary(listed.items)).toEqual({ ids: ['preset-a', 'preset-b', 'preset-c'], selected: ['preset-b'], applied: ['preset-c'] })
    })

    it.each([false, true])('reads a preset that is neither selected nor applied without its connection fields (catalog=%s)', async (catalog) => {
        const { stored } = await presetLibrary({ catalog, edit: (database) => {
            Object.assign(database.botPresets[2], { futureEndpoint: 'https://C.invalid/future' })
        } })
        const api = plugin()

        const result = await api.readPreset({ id: 'preset-c' })

        const record = (await stored('preset-c'))!
        const hidden = [...connectionFields, 'futureEndpoint']
        expect(result).toEqual({ status: 'ok', preset: Object.fromEntries(Object.entries(record).filter(([field]) => !hidden.includes(field))) })
        expect(result).toMatchObject({ preset: { id: 'preset-c', name: 'C', mainPrompt: 'Prompt C', temperature: 12 } })
        await expect(api.readPreset({ id: 'preset-c' }, { fields: ['mainPrompt', 'openAIKey'] }))
            .resolves.toEqual({ status: 'ok', preset: { mainPrompt: 'Prompt C' } })
    })

    it.each([false, true])('reads edits the app has not saved yet (catalog=%s)', async (catalog) => {
        await presetLibrary({ catalog, bound: 'preset-c' })
        const api = plugin()
        settingsOpen.set(true)
        flushSync()
        // The settings screen edits the selection A while the conversation applies C.
        presetEditorDBState.db.mainPrompt = 'Unsaved A'
        getDatabase().temperature = 77

        await expect(api.readPreset({ id: 'preset-a' }, { fields: ['mainPrompt'] })).resolves.toEqual({ status: 'ok', preset: { mainPrompt: 'Unsaved A' } })
        await expect(api.readPreset({ id: 'preset-c' }, { fields: ['temperature'] })).resolves.toEqual({ status: 'ok', preset: { temperature: 77 } })
    })

    it.each([false, true])('updates a preset that is not applied, which shows once it is selected (catalog=%s)', async (catalog) => {
        const { stored } = await presetLibrary({ catalog })
        const api = plugin()

        await expect(api.updatePreset({ id: 'preset-b' }, { mainPrompt: 'Written B', name: 'Renamed B' })).resolves.toEqual({ status: 'applied' })

        await expect(api.readPreset({ id: 'preset-b' }, { fields: ['mainPrompt', 'name'] }))
            .resolves.toEqual({ status: 'ok', preset: { mainPrompt: 'Written B', name: 'Renamed B' } })
        expect(getDatabase().mainPrompt).toBe('Prompt A')
        expect(getDatabase().botPresets.map((preset) => preset.name)).toEqual(['A', 'Renamed B', 'C'])
        expect((await stored('preset-b'))!.proxyKey).toBe('proxy B')
        await expect(api.selectPreset({ id: 'preset-b' })).resolves.toEqual({ status: 'applied' })
        expect(getDatabase().mainPrompt).toBe('Written B')
    })

    it('shows an update of the applied preset in the settings screen', async () => {
        const { stored } = await presetLibrary()

        await expect(plugin().updatePreset({ id: 'preset-a' }, { temperature: 55 })).resolves.toEqual({ status: 'applied' })

        expect(presetEditorDBState.db.temperature).toBe(55)
        expect((await stored('preset-a'))!.temperature).toBe(55)
    })

    it.each([false, true])('shows an update of the selected preset in the open settings screen while another preset is applied (catalog=%s)', async (catalog) => {
        const { stored } = await presetLibrary({ catalog, bound: 'preset-c' })
        settingsOpen.set(true)
        flushSync()
        expect(presetEditorDBState.db.mainPrompt).toBe('Prompt A')

        await expect(plugin().updatePreset({ id: 'preset-a' }, { mainPrompt: 'Written A' })).resolves.toEqual({ status: 'applied' })

        expect(presetEditorDBState.db.mainPrompt).toBe('Written A')
        expect(getDatabase().mainPrompt).toBe('Prompt C')
        expect((await stored('preset-a'))!.mainPrompt).toBe('Written A')
    })

    it('writes a field the app holds apart to the applied record while the app keeps the held value', async () => {
        const held = { memory: 'held', emotion: '', translate: '', otherAx: '' }
        const { stored } = await presetLibrary({ edit: (database) => {
            database.doNotChangeSeperateModels = true
            database.seperateModels = held
        } })
        const written = { memory: 'written', emotion: '', translate: '', otherAx: '' }

        await expect(plugin().updatePreset({ id: 'preset-a' }, { seperateModels: written })).resolves.toEqual({ status: 'applied' })

        expect((await stored('preset-a'))!.seperateModels).toEqual(written)
        expect(getDatabase().seperateModels).toEqual(held)
    })

    it.each([
        [{ proxyKey: 'stolen' }, 'unavailable', 'proxyKey'],
        [{ mainPrompt: 'kept', futureEndpoint: 'https://x.invalid' }, 'unavailable', 'futureEndpoint'],
        [{ id: 'preset-x' }, 'read-only', 'id'],
        [{ mainPrompt: undefined }, 'type', 'mainPrompt'],
    ])('rejects %j before asking to write and stores nothing', async (set, reason, field) => {
        const { stored } = await presetLibrary()
        const before = await stored('preset-b')

        // A declined prompt would answer denied; the rejection comes first.
        await expect(plugin({ write: false }).updatePreset({ id: 'preset-b' }, set)).resolves.toEqual({ status: 'rejected', reason, field })
        await expect(plugin({ write: false }).createPreset({ name: 'New', fields: set })).resolves.toEqual({ status: 'rejected', reason, field })
        expect(await stored('preset-b')).toEqual(before)
    })

    it('rejects a field whose value changes the stored kind and writes no field of that update', async () => {
        const { stored } = await presetLibrary()
        const before = await stored('preset-b')

        await expect(plugin().updatePreset({ id: 'preset-b' }, { mainPrompt: 'Not written', temperature: 'hot' }))
            .resolves.toEqual({ status: 'rejected', reason: 'type', field: 'temperature' })
        expect(await stored('preset-b')).toEqual(before)
    })

    it('creates a copy holding the source connection fields, and a preset from the default, without selecting either', async () => {
        const { store, stored } = await presetLibrary({ catalog: true })
        const api = plugin()

        const copied = await api.createPreset({ name: 'Copy of B', copyOf: 'preset-b', fields: { mainPrompt: 'Copied B' } })
        const created = await api.createPreset({ name: 'Fresh' })

        if (copied.status !== 'applied' || created.status !== 'applied') throw new Error(`Not created: ${JSON.stringify([copied, created])}`)
        expect(new Set(['preset-a', 'preset-b', 'preset-c', copied.id, created.id]).size).toBe(5)
        const source = (await stored('preset-b'))!
        const copy = (await stored(copied.id))!
        expect(Object.fromEntries(connectionFields.map((field) => [field, copy[field]])))
            .toEqual(Object.fromEntries(connectionFields.map((field) => [field, source[field]])))
        expect(copy).toMatchObject({ name: 'Copy of B', mainPrompt: 'Copied B', temperature: 11 })
        expect((await stored(created.id))!).toMatchObject({ name: 'Fresh', mainPrompt: presetTemplate.mainPrompt, aiModel: presetTemplate.aiModel })
        const listed = await api.listPresets()
        expect(summary(listed.items)).toEqual({ ids: ['preset-a', 'preset-b', 'preset-c', copied.id, created.id], selected: ['preset-a'], applied: ['preset-a'] })
        expect((await store.readRoot()).value.botPresetsId).toBe('preset-a')
    })

    it('does not remove the only preset', async () => {
        const { stored } = await presetLibrary({ names: ['A'] })

        await expect(plugin().deletePreset({ id: 'preset-a' })).resolves.toEqual({ status: 'rejected', reason: 'last-preset' })
        expect(await stored('preset-a')).toBeDefined()
    })

    it.each([false, true])('selects and applies the first remaining preset when the selected one is deleted (catalog=%s)', async (catalog) => {
        const { stored } = await presetLibrary({ catalog, edit: (database) => { database.botPresetsId = 1 } })
        const api = plugin()

        await expect(api.deletePreset({ id: 'preset-b' })).resolves.toEqual({ status: 'applied' })

        expect(await stored('preset-b')).toBeUndefined()
        expect(summary((await api.listPresets()).items)).toEqual({ ids: ['preset-a', 'preset-c'], selected: ['preset-a'], applied: ['preset-a'] })
        expect(getDatabase().mainPrompt).toBe('Prompt A')
    })

    it.each([false, true])('keeps the settings screen edits of the previous preset when another is selected (catalog=%s)', async (catalog) => {
        const { stored } = await presetLibrary({ catalog })
        const api = plugin()
        settingsOpen.set(true)
        flushSync()
        presetEditorDBState.db.mainPrompt = 'Edited A'
        flushSync()

        await expect(api.selectPreset({ id: 'preset-b' })).resolves.toEqual({ status: 'applied' })

        expect((await stored('preset-a'))!.mainPrompt).toBe('Edited A')
        expect(summary((await api.listPresets()).items)).toMatchObject({ selected: ['preset-b'], applied: ['preset-b'] })
        expect(presetEditorDBState.db.mainPrompt).toBe('Prompt B')
    })

    it.each([false, true])('keeps edits made to the selection while a bound preset applies, and the binding, on selecting another (catalog=%s)', async (catalog) => {
        const { stored } = await presetLibrary({ catalog, bound: 'preset-c' })
        const api = plugin()
        settingsOpen.set(true)
        flushSync()
        presetEditorDBState.db.mainPrompt = 'Edited A'
        flushSync()

        await expect(api.selectPreset({ id: 'preset-b' })).resolves.toEqual({ status: 'applied' })

        expect((await stored('preset-a'))!.mainPrompt).toBe('Edited A')
        expect(summary((await api.listPresets()).items)).toMatchObject({ selected: ['preset-b'], applied: ['preset-c'] })
        expect(presetEditorDBState.db.mainPrompt).toBe('Prompt B')
        expect(getDatabase().mainPrompt).toBe('Prompt C')
    })

    it('answers not-found for an unknown ID and changes nothing', async () => {
        const { store } = await presetLibrary()
        const api = plugin()
        const before = await store.materializeDatabase((await store.readRoot()).revision)
        const target = { id: 'preset-missing' }

        await expect(api.readPreset(target)).resolves.toEqual({ status: 'not-found' })
        await expect(api.updatePreset(target, { mainPrompt: 'x' })).resolves.toEqual({ status: 'not-found' })
        await expect(api.createPreset({ name: 'x', copyOf: target.id })).resolves.toEqual({ status: 'not-found' })
        await expect(api.deletePreset(target)).resolves.toEqual({ status: 'not-found' })
        await expect(api.selectPreset(target)).resolves.toEqual({ status: 'not-found' })
        expect(await store.materializeDatabase((await store.readRoot()).revision)).toEqual(before)
    })

    it('answers denied for a declined write, and rejects a read without the db permission', async () => {
        const { stored } = await presetLibrary()
        const before = await stored('preset-b')

        await expect(plugin({ write: false }).updatePreset({ id: 'preset-b' }, { mainPrompt: 'x' })).resolves.toEqual({ status: 'denied' })
        expect(await stored('preset-b')).toEqual(before)
        await expect(plugin({ read: false }).listPresets()).rejects.toThrow('db permission')
        await expect(plugin({ read: false }).readPreset({ id: 'preset-b' })).rejects.toThrow('db permission')
    })

    it.each([
        ['readPreset', [{ id: '' }]],
        ['readPreset', [{ id: 'preset-a' }, { fields: 'mainPrompt' }]],
        ['updatePreset', [{ id: 'preset-a' }, ['mainPrompt']]],
        ['createPreset', [{ copyOf: 'preset-a' }]],
        ['createPreset', [{ name: 'x', copyOf: 3 }]],
        ['createPreset', [{ name: 'x', fields: [] }]],
        ['deletePreset', ['preset-a']],
        ['selectPreset', [undefined]],
    ] as const)('rejects a malformed %s call with a TypeError before any prompt', async (method, args) => {
        const api = plugin({ read: false, write: false }) as unknown as Record<string, (...input: unknown[]) => Promise<unknown>>
        await expect(api[method](...args)).rejects.toThrow(TypeError)
    })
})

