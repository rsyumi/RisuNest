import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { Database } from '../../storage/database.svelte'

const fixture = vi.hoisted(() => ({
    api: null as Record<string, (...args: any[]) => any> | null,
    selectedIndex: 0,
    databaseAccessDependencies: null as null | { getSelectedCharacterId(): string | null },
    pluginPermissionReads: vi.fn(),
    readImage: vi.fn(),
    permissionValues: new Map<string, unknown>(),
    replacers: new Map<string, Set<Function>>(),
    listeners: new Set<Function>(),
    scopedAccess: {
        getFullObjectSnapshotStream: async (target: { characterIndex?: number; chatIndex?: number }, context: unknown) => {
            const access = fixture.scopedAccess
            const value = await (target.characterIndex === undefined
                ? access.getCurrentCharacter(context)
                : target.chatIndex === undefined
                    ? access.getCharacterFromIndex(target.characterIndex, context)
                    : access.getChatFromIndex(target.characterIndex, target.chatIndex, context))
            return value
        },
        getCurrentCharacter: vi.fn(),
        getCharacterFromIndex: vi.fn(),
        getChatFromIndex: vi.fn(),
        setCurrentCharacter: vi.fn(),
        setCharacterToIndex: vi.fn(),
        setChatToIndex: vi.fn(),
        setDatabase: vi.fn(),
        setDatabaseLite: vi.fn(),
    },
    database: {
        characters: [
            {
                type: 'character', chaId: 'active', name: 'Active', chatPage: 0,
                chats: [{ id: 'active-chat', name: 'Live', message: [{ role: 'char', data: 'a' }] }],
            },
            {
                type: 'character', chaId: 'trashed', name: 'Trash', trashTime: 1, chatPage: 0,
                chats: [{ id: 'trash-chat', name: 'Trash chat', message: [{ role: 'user', data: 'b' }] }],
            },
        ],
        plugins: [{ name: 'contract-plugin', script: '' }],
    } as unknown as Database,
}))

vi.mock('../plugins.svelte', async () => {
    const host = await import('./v3Host.testSupport')
    const apis = host.v2PluginApis({
        getChar: () => {
            const character = fixture.database.characters[fixture.selectedIndex]
            return character === undefined ? undefined : structuredClone(character)
        },
        setChar: (character: Database['characters'][number]) => {
            if (fixture.database.characters[fixture.selectedIndex]) {
                fixture.database.characters[fixture.selectedIndex] = character
            }
        },
        addRisuChatListener: (_mode: string, listener: Function) => fixture.listeners.add(listener),
        removeRisuChatListener: (_mode: string, listener: Function) => fixture.listeners.delete(listener),
        getArg: (arg: string) => { const [name, key] = arg.split('::'); return fixture.database.plugins.find((plugin) => plugin.name === name)?.realArg[key] },
        setArg: (arg: string, value: string) => { const [name, key] = arg.split('::'); const plugin = fixture.database.plugins.find((plugin) => plugin.name === name); if (plugin) plugin.realArg[key] = value },
        addRisuReplacer: (name: string, callback: Function) => { const callbacks = fixture.replacers.get(name) ?? new Set(); callbacks.add(callback); fixture.replacers.set(name, callbacks) },
        removeRisuReplacer: (name: string, callback: Function) => fixture.replacers.get(name)?.delete(callback),
        safeLocalStorage: {
            getItem: vi.fn(), setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(),
            key: vi.fn(), keys: vi.fn(), length: vi.fn(),
        },
        readImage: fixture.readImage,
    })
    return host.pluginsModule({
        getV2PluginAPIs: () => apis,
        pluginV2: { providers: new Map(), providerOptions: new Map() },
    })
})
vi.mock('./factory', async () => (await import('./v3Host.testSupport')).sandboxHostModule((api) => { fixture.api = api }))
vi.mock('src/ts/storage/database.svelte', async () => (await import('./v3Host.testSupport')).databaseModule({
    read: () => fixture.database,
    write: (database) => { fixture.database = database },
    selectedIndex: () => fixture.selectedIndex,
}))
vi.mock('../pluginSafeClass', async () => (await import('./v3Host.testSupport')).pluginSafeClassModule())
vi.mock('src/ts/stores.svelte', async () => (await import('./v3Host.testSupport')).storesModule({
    read: () => fixture.database,
    write: (database) => { fixture.database = database },
    selectedIndex: () => fixture.selectedIndex,
}))
vi.mock('src/ts/alert', async () => (await import('./v3Host.testSupport')).alertModule())
vi.mock('src/ts/util', async () => (await import('./v3Host.testSupport')).utilModule())
vi.mock('src/ts/globalApi.svelte', () => ({ checkCharOrder: vi.fn(), forageStorage: {}, getFetchLogs: vi.fn() }))
vi.mock('src/ts/gui/colorscheme', async () => (await import('./v3Host.testSupport')).colorschemeModule())
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('src/ts/process/mcp/pluginmcp', async () => (await import('./v3Host.testSupport')).pluginMcpModule())
vi.mock('src/ts/process/files/inlays', async () => (await import('./v3Host.testSupport')).inlaysModule())
vi.mock('src/ts/translator/translator', async () => (await import('./v3Host.testSupport')).translatorModule())
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule({ hasher: vi.fn(async (bytes: Uint8Array) => 'hash:' + new TextDecoder().decode(bytes)) }))
vi.mock('localforage', async () => (await import('../localforage.testSupport')).localforageModule(fixture.permissionValues, { getItem: fixture.pluginPermissionReads }))
vi.mock('src/ts/process/index.svelte', async () => (await import('./v3Host.testSupport')).processIndexModule())
vi.mock('src/ts/model/modellist', async () => (await import('./v3Host.testSupport')).modellistModule())
vi.mock('src/ts/process/request/request', async () => (await import('./v3Host.testSupport')).requestModule())
vi.mock('src/ts/process/modules', async () => (await import('./v3Host.testSupport')).modulesModule())
vi.mock('src/ts/process/ttsHooks', async () => (await import('./v3Host.testSupport')).ttsHooksModule())
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => (await import('./v3Host.testSupport')).persistentRuntimeModule())
vi.mock('../pluginDatabaseAccess', async () => (await import('./v3Host.testSupport')).pluginDatabaseAccessModule({
    createProductionPluginDatabaseAccess: vi.fn((dependencies) => {
        fixture.databaseAccessDependencies = dependencies
        return fixture.scopedAccess
    }),
}))

import { executePluginV3, loadV3Plugins, resetAllPluginPermissions, customV3ProviderMetaStore } from './v3.svelte'
import { alertConfirm } from 'src/ts/alert'
import { getInlayAsset } from 'src/ts/process/files/inlays'
import { customProviderStore, pluginV2 } from '../plugins.svelte'
import { get } from 'svelte/store'
import { getPluginPermissionStore } from 'src/ts/storage/nativePluginPermissions'
import { language } from 'src/lang'
import { linkPluginQueryAbortSignals } from '../pluginDatabaseAccess'

function resetDatabase(): void {
    fixture.database = {
        characters: [
            {
                type: 'character', chaId: 'active', name: 'Active', chatPage: 0,
                chats: [{ id: 'active-chat', name: 'Live', message: [{ role: 'char', data: 'a' }] }],
            },
            {
                type: 'character', chaId: 'trashed', name: 'Trash', trashTime: 1, chatPage: 0,
                chats: [{ id: 'trash-chat', name: 'Trash chat', message: [{ role: 'user', data: 'b' }] }],
            },
        ],
        plugins: [{ name: 'contract-plugin', script: '' }],
    } as unknown as Database
}

describe('Plugin v3 full-object access routing', () => {
    let pluginName = ''
    beforeEach(async () => {
        vi.clearAllMocks()
        resetDatabase()
        fixture.api = null
        fixture.selectedIndex = 0
        fixture.listeners.clear()
        fixture.scopedAccess.getCurrentCharacter.mockReset()
        fixture.scopedAccess.getCharacterFromIndex.mockReset()
        fixture.scopedAccess.getChatFromIndex.mockReset()
        fixture.scopedAccess.setCurrentCharacter.mockReset()
        fixture.scopedAccess.setCharacterToIndex.mockReset()
        fixture.scopedAccess.setChatToIndex.mockReset()
        fixture.scopedAccess.setDatabase.mockReset()
        fixture.scopedAccess.setDatabaseLite.mockReset()
        fixture.permissionValues.clear()
        vi.mocked(alertConfirm).mockResolvedValue(true)
        pluginName = `contract-plugin-${crypto.randomUUID()}`
        await executePluginV3({ name: pluginName, script: '' } as any)
    })

    it('routes getters through scoped access', async () => {
        const active = structuredClone(fixture.database.characters[0])
        const trashed = structuredClone(fixture.database.characters[1])
        fixture.scopedAccess.getCurrentCharacter.mockResolvedValue(active)
        fixture.scopedAccess.getCharacterFromIndex.mockResolvedValue(trashed)
        fixture.scopedAccess.getChatFromIndex.mockResolvedValue(trashed.chats[0])
        const api = fixture.api!

        await expect(api.getCharacter()).resolves.toMatchObject({ chaId: 'active' })
        await expect(api.getCharacterFromIndex(1)).resolves.toMatchObject({ chaId: 'trashed' })
        await expect(api.getChatFromIndex(1, 0)).resolves.toMatchObject({ id: 'trash-chat' })

        expect(fixture.pluginPermissionReads).not.toHaveBeenCalledWith(
            expect.stringContaining('_db'),
        )

        expect(fixture.scopedAccess.getCurrentCharacter.mock.calls[0][0]).toMatchObject({
            pluginName: expect.stringContaining('contract-plugin-'),
            signal: expect.any(AbortSignal),
        })
        expect(fixture.scopedAccess.getCharacterFromIndex).toHaveBeenCalledWith(
            1,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.scopedAccess.getChatFromIndex).toHaveBeenCalledWith(
            1,
            0,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
    })

    it('resolves a selected character independently of conversation validity', async () => {
        fixture.scopedAccess.getCurrentCharacter.mockResolvedValue(
            structuredClone(fixture.database.characters[0]),
        )
        await fixture.api!.getCharacter()
        fixture.database.characters[0].chats = []
        fixture.database.characters[0].chatPage = 99

        expect(fixture.databaseAccessDependencies?.getSelectedCharacterId()).toBe('active')
    })

    it('routes setters through scoped access', async () => {
        const api = fixture.api!
        const character = structuredClone(fixture.database.characters[1])
        const chat = structuredClone(fixture.database.characters[1].chats[0])

        await api.setChar(character)
        await api.setCharacter(character)
        await api.setCharacterToIndex(1, character)
        await api.setChatToIndex(1, 0, chat)

        expect(fixture.scopedAccess.setCurrentCharacter).toHaveBeenCalledTimes(2)
        expect(fixture.scopedAccess.setCurrentCharacter).toHaveBeenCalledWith(
            character,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.scopedAccess.setCharacterToIndex).toHaveBeenCalledWith(
            1,
            character,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.scopedAccess.setChatToIndex).toHaveBeenCalledWith(
            1,
            0,
            chat,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.database.characters[1].name).toBe('Trash')
        expect(fixture.pluginPermissionReads).not.toHaveBeenCalledWith(
            expect.stringMatching(/_db$/),
        )
    })

    it.each([true, false])('asks for dbWrite once and writes only when it is granted (%s)', async (granted) => {
        vi.mocked(alertConfirm).mockResolvedValue(granted)
        const api = fixture.api!
        const character = structuredClone(fixture.database.characters[1])
        const writes = fixture.scopedAccess

        await expect(api.setChar(character)).resolves.toBeUndefined()
        await expect(api.setCharacter(character)).resolves.toBeUndefined()
        await expect(api.setCharacterToIndex(1, character)).resolves.toBeUndefined()
        await expect(api.setChatToIndex(1, 0, character.chats[0])).resolves.toBeUndefined()
        await expect(api.setDatabase({ username: 'synthetic' })).resolves.toBeUndefined()
        await expect(api.setDatabaseLite({ username: 'synthetic' })).resolves.toBeUndefined()

        expect(alertConfirm).toHaveBeenCalledOnce()
        expect(alertConfirm).toHaveBeenCalledWith(language.dbWriteConsent.replace('{}', pluginName))
        expect([
            writes.setCurrentCharacter, writes.setCharacterToIndex, writes.setChatToIndex,
            writes.setDatabase, writes.setDatabaseLite,
        ].map((write) => write.mock.calls.length)).toEqual(granted ? [2, 1, 1, 1, 1] : [0, 0, 0, 0, 0])
    })

    it('rejects risunestPatchConversation when dbWrite is declined', async () => {
        vi.mocked(alertConfirm).mockResolvedValue(false)
        vi.mocked(linkPluginQueryAbortSignals).mockImplementation(() => ({ signal: new AbortController().signal, dispose() {} }))

        await expect(fixture.api!.risunestPatchConversation({
            characterId: 'active',
            conversationId: 'active-chat',
            mutationId: 'declined',
            messages: [{ index: 0, messageId: 'm0', set: { __tr: 'text' } }],
        })).rejects.toThrow('Conversation patches require the dbWrite permission')

        expect(alertConfirm).toHaveBeenCalledOnce()
        expect(alertConfirm).toHaveBeenCalledWith(language.dbWriteConsent.replace('{}', pluginName))
    })
})


describe('Plugin v3 runtime consent and ownership', () => {
    beforeEach(async () => {
        await loadV3Plugins([])
        fixture.permissionValues.clear()
        fixture.pluginPermissionReads.mockImplementation(async (key: string) => fixture.permissionValues.get(key) ?? null)
        vi.mocked(alertConfirm).mockReset().mockResolvedValue(true)
        fixture.replacers.clear()
        fixture.database.plugins = [
            { name: 'owner', script: 'original', realArg: { marker: 'own' } },
            { name: 'other', script: 'other-code', realArg: { marker: 'foreign' } },
        ] as any
    })
    afterEach(async () => { await loadV3Plugins([]); vi.restoreAllMocks() })
    async function start(name = 'owner', script = 'original') {
        await executePluginV3({ name, script } as any)
        return fixture.api!
    }

    it('binds deprecated arguments to the executing owner', async () => {
        const api = await start()
        expect(api.getArg('owner::marker')).toBe('own')
        api.setArg('owner::marker', 'updated')
        expect(api.getArg('owner::marker')).toBe('updated')
        expect(api.getArg('other::marker')).toBeUndefined()
        api.setArg('other::marker', 'stolen')
        expect(fixture.database.plugins[1].realArg.marker).toBe('foreign')
    })

    it('hashes executing code rather than a mutable installed record', async () => {
        const api = await start()
        fixture.database.plugins[0].script = 'granted'
        fixture.permissionValues.set('hash:granted_mainDom', true)
        vi.mocked(alertConfirm).mockResolvedValue(false)
        expect(await api.requestPluginPermission('mainDom')).toBe(false)
        expect(alertConfirm).toHaveBeenCalledOnce()
    })

    it('requires a fresh grant when the same name executes changed source', async () => {
        const first = await start()
        expect(await first.requestPluginPermission('mainDom')).toBe(true)
        await loadV3Plugins([])
        const next = await start('owner', 'changed')
        expect(await next.requestPluginPermission('mainDom')).toBe(true)
        expect(alertConfirm).toHaveBeenCalledTimes(2)
    })

    it('waits for an in-flight reset before reading grants into a new epoch', async () => {
        const api = await start()
        await api.requestPluginPermission('mainDom')
        const store = getPluginPermissionStore()
        const original = store.clearAll.bind(store)
        let release!: () => void
        const gate = new Promise<void>((resolve) => { release = resolve })
        const clear = vi.spyOn(store, 'clearAll').mockImplementation(async () => { await gate; await original() })
        const reset = resetAllPluginPermissions()
        await vi.waitFor(() => expect(clear).toHaveBeenCalledOnce())
        let settled = false
        const permission = api.requestPluginPermission('mainDom').then((value) => { settled = true; return value })
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(settled).toBe(false)
        release()
        await reset
        expect(await permission).toBe(true)
        expect(alertConfirm).toHaveBeenCalledTimes(2)
    })

    it('drops denied runtime decisions when the same name loads new code', async () => {
        vi.mocked(alertConfirm).mockResolvedValue(false)
        const first = await start()
        expect(await first.requestPluginPermission('mainDom')).toBe(false)
        expect(await first.requestPluginPermission('mainDom')).toBe(false)
        await loadV3Plugins([])
        vi.mocked(alertConfirm).mockResolvedValue(true)
        const next = await start('owner', 'changed')
        expect(await next.requestPluginPermission('mainDom')).toBe(true)
        expect(alertConfirm).toHaveBeenCalledTimes(2)
    })

    it.each(['db', 'dbWrite'])('honors periodic upfront %s grants across reload and rechecks at the deadline', async (permission) => {
        let now = 1000
        vi.spyOn(Date, 'now').mockImplementation(() => now)
        const first = await start()
        expect(await first.requestPluginPermission(permission)).toBe(true)
        await loadV3Plugins([])
        const next = await start()
        expect(await next.requestPluginPermission(permission)).toBe(true)
        expect(alertConfirm).toHaveBeenCalledOnce()
        now += 7 * 24 * 60 * 60 * 1000 - 1
        expect(await next.requestPluginPermission(permission)).toBe(true)
        expect(alertConfirm).toHaveBeenCalledOnce()
        now += 1
        expect(await next.requestPluginPermission(permission)).toBe(true)
        expect(alertConfirm).toHaveBeenCalledTimes(2)
        await resetAllPluginPermissions()
        expect(await next.requestPluginPermission(permission)).toBe(true)
        expect(alertConfirm).toHaveBeenCalledTimes(3)
    })

    it('removes both replacer kinds and rejects registrations after unload', async () => {
        const api = await start()
        const callback = vi.fn()
        await api.addRisuReplacer('beforeRequest', callback)
        await api.addRisuReplacer('afterRequest', callback)
        expect(fixture.replacers.get('beforeRequest')?.size).toBe(1)
        await loadV3Plugins([])
        expect(fixture.replacers.get('beforeRequest')?.size).toBe(0)
        expect(fixture.replacers.get('afterRequest')?.size).toBe(0)
        const next = await start()
        let approve!: (value: boolean) => void
        await resetAllPluginPermissions()
        vi.mocked(alertConfirm).mockImplementation(() => new Promise((resolve) => { approve = resolve }))
        const pending = next.addRisuReplacer('beforeRequest', callback)
        await vi.waitFor(() => expect(approve).toBeTypeOf('function'))
        await loadV3Plugins([])
        approve(true)
        await pending
        expect(fixture.replacers.get('beforeRequest')?.size).toBe(0)
    })

    it('ends the writes waiting on the dbWrite question when the plugin unloads', async () => {
        const api = await start()
        vi.mocked(alertConfirm).mockImplementation(() => new Promise(() => {}))
        vi.mocked(linkPluginQueryAbortSignals).mockImplementation(() => ({ signal: new AbortController().signal, dispose() {} }))
        const write = api.setDatabaseLite({ username: 'synthetic' })
        const patch = api.risunestPatchConversation({
            characterId: 'active',
            conversationId: 'active-chat',
            mutationId: 'unloaded',
            messages: [{ index: 0, messageId: 'm0', set: { __tr: 'text' } }],
        })
        await vi.waitFor(() => expect(alertConfirm).toHaveBeenCalled())

        await loadV3Plugins([])

        await expect(write).resolves.toBeUndefined()
        await expect(patch).rejects.toThrow()
        expect(fixture.scopedAccess.setDatabaseLite).not.toHaveBeenCalled()
    })

    it('replaces provider metadata and cleans up the currently owned registration', async () => {
        const api = await start()
        api.addProvider('provider', vi.fn(), { model: { name: 'First' } })
        api.addProvider('provider', vi.fn(), { model: { name: 'Latest' } })
        expect(get(customProviderStore)).toEqual(['provider'])
        expect(customV3ProviderMetaStore.filter((model) => model.id === 'pluginmodel:::provider').map((model) => model.name)).toEqual(['Latest'])
        await loadV3Plugins([])
        expect(get(customProviderStore)).toEqual([])
        expect(customV3ProviderMetaStore).toEqual([])
        expect(pluginV2.providerOptions.has('provider')).toBe(false)
        expect(pluginV2.providers.has('provider')).toBe(false)
    })

    it('reads assets and Inlays behind the app\'s own reads', async () => {
        const api = await start()
        await api.readImage('portrait.png')
        expect(fixture.readImage).toHaveBeenLastCalledWith('portrait.png', { background: true })
        await api.readInlay('inlay-id')
        expect(getInlayAsset).toHaveBeenLastCalledWith('inlay-id', { background: true })
    })
})
