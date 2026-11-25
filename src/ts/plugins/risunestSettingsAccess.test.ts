import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { get } from 'svelte/store'
import { describe, expect, it, vi } from 'vitest'
import type { SettingItem } from '../setting/types'
import type { Database } from '../storage/database.svelte'
import type { PersistentDataStore } from '../storage/persistentDataStore'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import { fixtureDatabase } from '../storage/tests/persistentDataFixtures'
import { risunestSettingKeyClasses } from './risunestSettingKeys'

const state = vi.hoisted(() => ({ store: null as PersistentDataStore | null }))

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
vi.mock('../storage/persistentDataStoreFactory', async (importOriginal) => ({
    ...await importOriginal<typeof import('../storage/persistentDataStoreFactory')>(),
    getPersistentDataStore: () => state.store,
    getRawPersistentDataStore: () => state.store,
}))

// `theme` is one of the keys the generic database calls reach.
const genericKeys = ['theme']
const globalRegex = [{ comment: 'Plugin rule', in: 'a', out: 'b', type: 'editdisplay', ableFlag: false }]

/// Boots the production runtime on a fresh store and opens the settings methods of one plugin
/// over it. The runtime is a module singleton bound to the first store it sees, so every call
/// loads a fresh module graph.
async function openSettings(name: string) {
    vi.resetModules()
    // stores.svelte loads before database.svelte, as the app loads them.
    const stores = await import('../stores.svelte')
    const database = await import('../storage/database.svelte')
    const runtime = await import('../storage/persistentDataRuntime.svelte')
    const { projectCompleteScalableWorkingSet } = await import('../storage/workingSetCatalog')
    const { createPluginDatabaseAccess } = await import('./pluginDatabaseAccess')
    const { createRisunestSettingsAccess } = await import('./risunestSettingsAccess')
    const lang = await import('src/lang')

    const initial = database.normalizeDatabaseDefaults(structuredClone(fixtureDatabase) as Database)
    const store = new IndexedDbPersistentDataStore(name, new IDBFactory(), IDBKeyRange)
    await store.open()
    state.store = store
    const { revision } = await store.replaceFromDatabase(initial)
    database.setDatabaseLite(projectCompleteScalableWorkingSet(initial, null, revision, new Set()))
    await runtime.getPersistentDataRuntime().initializeActiveWorkingSet(database.getDatabase())

    const permission = { db: true, dbWrite: true, asked: 0 }
    const lifetime = new AbortController()
    // The plugin's generic database calls, wired to the same runtime as the host wires them.
    const databaseAccess = createPluginDatabaseAccess({
        owner: 'settings-plugin',
        store,
        getPersistentRevision: runtime.getPersistentRevision,
        commitPersistentUnitIntent: runtime.commitPersistentUnitIntent,
        flushPendingData: runtime.flushPendingDataLocally,
        getCompatibilityDatabase: () => stores.DBState.db,
        getSelectedCharacterId: () => null,
        captureSelectedConversationTarget: runtime.captureSelectedConversationTarget,
        acquireCompleteConversation: runtime.acquireCompleteConversation,
        refreshSelectedConversationAfterReplacement: runtime.refreshSelectedConversationAfterReplacement,
        reportIdentityReplacementRejected: () => {},
        getNavigationGeneration: runtime.getPersistentNavigationGeneration,
        getStorageAuthorityEpoch: runtime.getPersistentStorageAuthorityEpoch,
        assertPersistentMutationAllowed: runtime.assertPersistentMutationAllowed,
        readPluginStorageSnapshot: async () => ({}),
        snapshot: (value) => JSON.parse(JSON.stringify(value)),
    })
    const settings = createRisunestSettingsAccess({
        pluginName: 'settings-plugin',
        databaseAccess,
        hasDatabasePermission: async () => (permission.asked++, permission.db),
        hasDatabaseWritePermission: async () => (permission.asked++, permission.dbWrite),
        lifetimeSignal: lifetime.signal,
    })
    const stored = async () => (await store.readRoot()).value as unknown as Record<string, unknown>
    return {
        settings,
        databaseAccess,
        permission,
        stored,
        live: () => stores.DBState.db as unknown as Record<string, unknown>,
        flush: () => runtime.flushPendingDataLocally('test-app-edit'),
        lang,
    }
}

// The first case transforms the whole runtime graph after resetting modules.
describe('settings by key', { timeout: 60_000 }, () => {
    it('lists every offered key once with its kind and the settings screen description, without asking', async () => {
        const { settings, permission, lang } = await openSettings('settings-list')
        permission.db = false

        const { items, total } = await settings.listSettingKeys()

        const offered = [...risunestSettingKeyClasses].filter(([, entry]) => entry.access !== 'excluded').map(([key]) => key)
        expect(items.map((item) => item.key)).toEqual(offered)
        expect(total).toBe(offered.length)
        expect(items.map((item) => item.key)).not.toContain('openAIKey')
        expect(items.map((item) => item.key)).not.toContain('keiServerURL')
        expect(permission.asked).toBe(0)
        const item = (key: string) => items.find((value) => value.key === key)
        expect(item('loreBookDepth')).toEqual({
            key: 'loreBookDepth', type: 'number', nullable: true, writable: true,
            label: lang.language.loreBookDepth, min: 0, max: 20,
        })
        expect(item('gptVisionQuality')).toMatchObject({
            type: 'string',
            label: 'Vision Quality',
            help: lang.language.help.gptVisionQuality,
            options: [{ value: 'low', label: 'Low' }, { value: 'high', label: 'High' }],
        })
        expect(item('risunestInlayFormat')?.options).toEqual([
            { value: 'webp', label: lang.language.risuNest.inlay.formatWebp },
            { value: 'png', label: lang.language.risuNest.inlay.formatPng },
            { value: 'original', label: lang.language.risuNest.inlay.formatOriginal },
        ])
        expect(item('loreBook')).toEqual({ key: 'loreBook', type: 'array', nullable: false, writable: true })
        // A control whose value goes through a custom accessor describes nothing.
        expect(item('risunestInlayAnimationMaxFps')).toEqual({
            key: 'risunestInlayAnimationMaxFps', type: 'number', nullable: false, writable: true,
        })
        expect(item('botPresetsId')).toEqual({ key: 'botPresetsId', type: 'number', nullable: false, writable: false })

        // Text follows the app's current language.
        lang.changeLanguage('ko')
        const korean = await settings.listSettingKeys()
        expect(korean.items.find((value) => value.key === 'loreBookDepth')?.label).toBe(lang.language.loreBookDepth)
        expect(lang.language.loreBookDepth).not.toBe(item('loreBookDepth')?.label)
    })

    it('does what the settings screen does after a change, such as switching the language', async () => {
        const { settings, lang } = await openSettings('settings-effects')
        const label = async () => (await settings.listSettingKeys()).items.find((value) => value.key === 'loreBookDepth')?.label
        const english = await label()

        await expect(settings.writeSettings({ language: 'ko', sideBarSize: 2 })).resolves.toEqual({ status: 'applied' })

        expect(await label()).toBe(lang.language.loreBookDepth)
        expect(lang.language.loreBookDepth).not.toBe(english)
        expect(document.documentElement.style.getPropertyValue('--sidebar-size')).toBe('32rem')
    })

    it('answers applied when what follows a stored change fails, and shows the failure as the settings screen does', async () => {
        const window = { isFullscreen: vi.fn(async () => false), setFullscreen: vi.fn(async () => { throw new Error('window unavailable') }) }
        vi.doMock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'tauri', os: 'windows' }))
        vi.doMock('@tauri-apps/api/webviewWindow', () => ({ getCurrentWebviewWindow: () => window }))
        try {
            const { settings, stored } = await openSettings('settings-effect-failure')
            const { alertStore } = await import('../stores.svelte')

            await expect(settings.writeSettings({ fullScreen: true })).resolves.toEqual({ status: 'applied' })

            expect((await stored()).fullScreen).toBe(true)
            expect(get(alertStore)).toMatchObject({ type: 'error', msg: 'window unavailable' })
        } finally {
            vi.doUnmock('../platform')
            vi.doUnmock('@tauri-apps/api/webviewWindow')
        }
    })

    it('applies a write of every key whose settings screen control does more than store it', async () => {
        const { loadSettingDefinitions } = await import('./risunestSettingDescriptions')
        const { risunestSettingEffects } = await import('./risunestSettingsRuntime.svelte')
        const changing: string[] = []
        const visit = (items: readonly SettingItem[]) => {
            for (const item of items) {
                if (item.bindKey && item.onChange) changing.push(item.bindKey)
                if (item.options?.children) visit(item.options.children)
            }
        }
        for (const items of (await loadSettingDefinitions()).groups) visit(items)

        expect(changing).toContain('language')
        expect(changing.filter((key) => !Object.hasOwn(risunestSettingEffects, key))).toEqual([])
    })

    it('reads available keys with unsaved app edits and lists the rest as unavailable', async () => {
        const { settings, permission, stored, live } = await openSettings('settings-read')
        live().askRemoval = !live().askRemoval

        const result = await settings.readSettings(['loreBook', 'openAIKey', 'nope', 'askRemoval', 'textScreenColor', 'botPresetsId', 'selectedPersona'])

        expect(result.unavailable).toEqual(['openAIKey', 'nope'])
        expect(result.values.loreBook).toEqual(live().loreBook)
        expect(result.values.askRemoval).toBe(live().askRemoval)
        expect((await stored()).askRemoval).toBe(live().askRemoval)
        // A key without a value has no entry.
        expect(Object.hasOwn(result.values, 'textScreenColor')).toBe(false)
        // The preset and persona selections read as the indexes the app keeps, not as stored IDs.
        expect(result.values).toMatchObject({ botPresetsId: 0, selectedPersona: 0 })
        expect(Object.keys(result.values)).toEqual(['loreBook', 'askRemoval', 'botPresetsId', 'selectedPersona'])

        permission.db = false
        await expect(settings.readSettings(['loreBook'])).rejects.toThrow('requires the db permission')
        await expect(settings.readSettings('loreBook')).rejects.toThrow(TypeError)
        await expect(settings.readSettings(['loreBook', 1])).rejects.toThrow(TypeError)
    })

    it('replaces a whole value in one commit that the app shows, keeping every other value', async () => {
        const { settings, stored, live } = await openSettings('settings-write')
        // An app edit not yet saved when the plugin writes.
        live().askRemoval = !live().askRemoval
        const before = await stored()

        await expect(settings.writeSettings({ globalscript: globalRegex })).resolves.toEqual({ status: 'applied' })

        const after = await stored()
        expect(after.globalscript).toEqual(globalRegex)
        // The regex settings screen binds this value.
        expect(live().globalscript).toEqual(globalRegex)
        expect((await settings.readSettings(['globalscript'])).values.globalscript).toEqual(globalRegex)
        expect(after.askRemoval).toBe(live().askRemoval)
        const { globalscript: _written, askRemoval: _edited, ...rest } = after
        const { globalscript: _old, askRemoval: _unsaved, ...restBefore } = before
        expect(rest).toEqual(restBefore)
    })

    it('writes the value it was given even when it equals what the plugin read before an app edit', async () => {
        const { settings, stored, live, flush } = await openSettings('settings-unconditional')
        const read = (await settings.readSettings(['loreBookDepth'])).values.loreBookDepth
        live().loreBookDepth = (read as number) + 2
        await flush()
        expect((await stored()).loreBookDepth).toBe((read as number) + 2)

        await expect(settings.writeSettings({ loreBookDepth: read })).resolves.toEqual({ status: 'applied' })

        expect((await stored()).loreBookDepth).toBe(read)
        expect(live().loreBookDepth).toBe(read)
    })

    it('leaves the generic database calls of the same plugin as they were', async () => {
        const { settings, databaseAccess, stored, live, flush } = await openSettings('settings-generic')
        const original = (await databaseAccess.getDatabaseSnapshot(['theme'], genericKeys)).theme
        live().theme = 'waifu'
        await flush()

        expect((await settings.readSettings(['theme'])).values.theme).toBe('waifu')
        // The plugin submits the theme of its own earlier read unchanged, which changes nothing.
        await databaseAccess.setDatabaseLite({ theme: original }, genericKeys)

        expect((await stored()).theme).toBe('waifu')
        expect(live().theme).toBe('waifu')
    })

    it('rejects a write it can judge without stored data before asking, and writes nothing', async () => {
        const { settings, permission, stored } = await openSettings('settings-reject')
        const before = await stored()

        await expect(settings.writeSettings({ loreBookDepth: '5' })).resolves.toEqual({ status: 'rejected', reason: 'type', key: 'loreBookDepth' })
        await expect(settings.writeSettings({ loreBookDepth: 3, openAIKey: 'sk-test' })).resolves.toEqual({ status: 'rejected', reason: 'unavailable', key: 'openAIKey' })
        await expect(settings.writeSettings({ botPresetsId: 1 })).resolves.toEqual({ status: 'rejected', reason: 'read-only', key: 'botPresetsId' })
        await expect(settings.writeSettings({ askRemoval: null })).resolves.toEqual({ status: 'rejected', reason: 'type', key: 'askRemoval' })
        await expect(settings.writeSettings({ loreBook: undefined })).resolves.toEqual({ status: 'rejected', reason: 'type', key: 'loreBook' })
        await expect(settings.writeSettings(JSON.parse('{"__proto__": 1}'))).resolves.toEqual({ status: 'rejected', reason: 'unavailable', key: '__proto__' })
        await expect(settings.writeSettings({ constructor: 1 })).resolves.toEqual({ status: 'rejected', reason: 'unavailable', key: 'constructor' })
        await expect(settings.writeSettings([['loreBookDepth', 3]])).rejects.toThrow(TypeError)
        await expect(settings.writeSettings('loreBookDepth')).rejects.toThrow(TypeError)

        expect(permission.asked).toBe(0)
        expect(await stored()).toEqual(before)
    })

    it('answers denied and writes nothing when the user declines', async () => {
        const { settings, permission, stored } = await openSettings('settings-denied')
        permission.dbWrite = false
        const before = await stored()

        await expect(settings.writeSettings({ globalscript: globalRegex })).resolves.toEqual({ status: 'denied' })

        expect(permission.asked).toBe(1)
        expect(await stored()).toEqual(before)
    })

    it('stores null for a key that allows it and a selection index as its record', async () => {
        const { settings, stored, live } = await openSettings('settings-null')

        await expect(settings.writeSettings({ textScreenColor: '#102030' })).resolves.toEqual({ status: 'applied' })
        await expect(settings.writeSettings({ textScreenColor: null, loreBookDepth: null })).resolves.toEqual({ status: 'applied' })
        expect(await stored()).toMatchObject({ textScreenColor: null, loreBookDepth: null })
        expect(live().textScreenColor).toBeNull()
        // The app gives a nullish depth its default, as a load does.
        expect((await settings.readSettings(['loreBookDepth'])).values.loreBookDepth).toBe(5)

        const personas = live().personas as { id: string }[]
        await expect(settings.writeSettings({ selectedPersona: 0 })).resolves.toEqual({ status: 'applied' })
        expect((await stored()).selectedPersona).toBe(personas[0].id)
        expect((await settings.readSettings(['selectedPersona'])).values.selectedPersona).toBe(0)
    })
})
