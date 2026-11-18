import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    assets: new Map<string, Uint8Array>(),
    details: new Map<string, any>(),
    root: { characterOrder: [] as any[] },
    db: { characters: [] as any[], modules: [] as any[], enabledModules: [] as string[] },
    alertError: vi.fn(),
    alertConfirm: vi.fn(async () => true),
    missingTarget: false,
    /** Write reasons that fail. */
    failing: new Set<string>(),
}))
const platform = vi.hoisted(() => ({ runtime: 'web' as 'tauri' | 'web' }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({ runtime: platform.runtime, os: 'windows' })))
const core = vi.hoisted(() => ({ invoke: vi.fn(async (_command: string, _args?: unknown) => ({ revision: 2 })) }))
vi.mock('@tauri-apps/api/core', async (importOriginal) => ({ ...await importOriginal<typeof import('@tauri-apps/api/core')>(), invoke: core.invoke }))

vi.mock('../globalApi.svelte', () => ({
    loadAsset: async (key: string) => mocks.assets.get(key) ?? null,
    saveAsset: async (data: Uint8Array) => {
        const key = `assets/base-${mocks.assets.size + 1}.json`
        mocks.assets.set(key, data)
        return key
    },
}))
vi.mock('../alert', () => ({ alertClear: vi.fn(), alertError: mocks.alertError, alertConfirm: mocks.alertConfirm }))
vi.mock('../stores.svelte', () => ({ DBState: { db: mocks.db } }))
vi.mock('../characters', () => ({ updateLorebooks: (books: unknown[]) => books }))
vi.mock('../process/inlayScreen', () => ({ updateInlayScreen: (value: unknown) => value }))
vi.mock('../util', () => ({ selectSingleFile: vi.fn() }))
// The earlier-file reader's lorebook converter, without the app state its module loads.
vi.mock('../characterCards', () => ({ convertCharbook: vi.fn() }))
vi.mock('../storage/persistentDataRuntime.svelte', async () => ({
    assetAliasCleanupTarget: (await import('../storage/deletedOwnerAssetCleanup.testSupport')).createAssetAliasCleanupTarget,
    readPersistentCharacterDetail: async (id: string) => {
        const detail = mocks.details.get(id)
        if (!detail) return null
        const { chats: _chats, ...rest } = detail
        return structuredClone(rest)
    },
    mutatePersistentCharacterDetail: async (id: string, reason: string, mutate: (state: any) => void) => {
        const detail = mocks.details.get(id)
        if (mocks.failing.has(reason)) throw new Error('write failed')
        if (!detail) return false
        const character = structuredClone(detail)
        mutate({ root: mocks.root, character })
        mocks.details.set(id, editableDetail(detail, character))
        return true
    },
    mutateCharacterDetailFields: async (id: string, reason: string, mutate: (state: any) => void) => {
        const detail = mocks.details.get(id)
        if (mocks.failing.has(reason)) throw new Error('write failed')
        if (!detail || mocks.missingTarget) return false
        const character = structuredClone(detail)
        mutate({ character })
        mocks.details.set(id, editableDetail(detail, character))
        return true
    },
}))

import { language } from 'src/lang'
import { UPDATE_BASE_EXTENSION_KEY, markModuleImport, unresolvedItems } from './merge'
import {
    chooseContentUpdate,
    commitImportedCharacter,
    commitImportedModule,
    contentUpdateDialog,
    importAsContentUpdate,
    loadContentUpdateBaseFile,
    reviewContentUpdate,
    setContentUpdateChoice,
} from './flow.svelte'
import { selectSingleFile } from '../util'
import { registerGeneratingConversation } from '../storage/generatingConversationRegistry'
import type { RisuModule } from '../process/modules'

/** Like the native store, which refuses a detail write that touches these fields. */
function editableDetail(before: any, after: any) {
    for (const field of ['chaId', 'type', 'chats']) {
        if (JSON.stringify(before[field]) !== JSON.stringify(after[field])) throw new Error('unit-not-editable')
    }
    return after
}

function install(value: any) {
    mocks.details.set(value.chaId, structuredClone(value))
    mocks.db.characters.push(structuredClone(value))
    mocks.root.characterOrder.push(value.chaId)
}

const commitCharacter = vi.fn(async (value: any) => install(value))
const commitModule = vi.fn(async (value: RisuModule) => {
    mocks.db.modules.push(value)
})

function card(chaId: string, name: string, fields: Record<string, unknown> = {}) {
    return { type: 'character', chaId, name, chats: [{ id: `${chaId}-chat`, message: [] }], extentions: {}, ...fields } as any
}

/** The asset keys the cleanup after an update was asked to delete when unused. */
async function releasedKeys() {
    await vi.waitFor(() => expect(core.invoke).toHaveBeenCalledWith('pds_asset_delete_orphaned_aliases', expect.anything()))
    const call = core.invoke.mock.calls.find(([command]) => command === 'pds_asset_delete_orphaned_aliases')!
    return (call[1] as { owner: { keys: string[] } }).owner.keys
}

async function opened() {
    await vi.waitFor(() => expect(contentUpdateDialog.request).not.toBeNull())
}

async function reviewed() {
    await opened()
    await vi.waitFor(() => expect(contentUpdateDialog.step).toBe('review'))
}

beforeEach(() => {
    mocks.assets.clear()
    mocks.details.clear()
    mocks.root.characterOrder = []
    mocks.db.characters = []
    mocks.db.modules = []
    mocks.db.enabledModules = []
    mocks.missingTarget = false
    mocks.failing.clear()
    mocks.alertError.mockClear()
    mocks.alertConfirm.mockReset().mockResolvedValue(true)
    platform.runtime = 'web'
    core.invoke.mockClear()
    commitCharacter.mockClear()
    commitModule.mockClear()
})

afterEach(() => {
    if (contentUpdateDialog.request) chooseContentUpdate('cancel')
})

describe('character update', () => {
    it('installs an import that matches nothing and records it as the base', async () => {
        const incoming = card('new', 'Luna', { desc: 'd' })
        expect(await commitImportedCharacter(incoming, commitCharacter)).toBe('new')
        expect(contentUpdateDialog.request).toBeNull()
        const baseKey = mocks.details.get('new').extentions[UPDATE_BASE_EXTENSION_KEY]
        expect(JSON.parse(new TextDecoder().decode(mocks.assets.get(baseKey)))).toEqual({ name: 'Luna', desc: 'd', extentions: {} })
    })

    it('updates only what the author changed, keeps the user edits and chats, and trashes the old card', async () => {
        platform.runtime = 'tauri'
        await commitImportedCharacter(card('first', 'Luna v1.0', { desc: 'old', personality: 'calm', backgroundHTML: '<b>' }), commitCharacter)
        mocks.details.get('first').personality = 'brave'
        const firstBase = mocks.details.get('first').extentions[UPDATE_BASE_EXTENSION_KEY]
        // A stored import may not carry a type yet, unlike the installed card.
        const { type: _type, ...second } = card('second', 'Luna v1.1', { desc: 'new', personality: 'calm', backgroundHTML: undefined })
        const pending = commitImportedCharacter(second as any, commitCharacter)
        await opened()
        expect(contentUpdateDialog.request?.candidates.map((candidate) => candidate.id)).toEqual(['first'])
        await reviewContentUpdate()
        expect(unresolvedItems(contentUpdateDialog.plan!)).toEqual([])
        chooseContentUpdate('update')
        expect(await pending).toBe('first')

        const updated = mocks.details.get('first')
        expect(updated).toMatchObject({ name: 'Luna v1.1', desc: 'new', personality: 'brave', chats: [{ id: 'first-chat' }] })
        expect(updated).not.toHaveProperty('backgroundHTML')
        const backup = mocks.details.get('second')
        expect(backup).toMatchObject({ chaId: 'second', name: `Luna v1.0 ${language.risuNest.contentUpdate.backupSuffix}`, desc: 'old', personality: 'brave', chats: [{ id: 'second-chat' }] })
        expect(backup.trashTime).toEqual(expect.any(Number))
        expect(backup.extentions[UPDATE_BASE_EXTENSION_KEY]).toBeUndefined()
        expect(mocks.root.characterOrder).toEqual(['first'])
        const baseKey = updated.extentions[UPDATE_BASE_EXTENSION_KEY]
        expect(JSON.parse(new TextDecoder().decode(mocks.assets.get(baseKey)))).toMatchObject({ name: 'Luna v1.1', desc: 'new' })
        expect(await releasedKeys()).toContain(firstBase)
    })

    it('asks about every difference without a base and applies the picks', async () => {
        install(card('old', 'Luna', { desc: 'mine', scenario: 'same' }))
        const pending = commitImportedCharacter(card('next', 'Luna Beta', { desc: 'theirs', scenario: 'same' }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        expect(contentUpdateDialog.plan!.items.map((item) => [item.field, item.status])).toEqual([['name', 'differs'], ['desc', 'differs']])
        setContentUpdateChoice('name', 'mine')
        chooseContentUpdate('update')
        expect(contentUpdateDialog.request).not.toBeNull()
        setContentUpdateChoice('desc', 'theirs')
        chooseContentUpdate('update')
        expect(await pending).toBe('old')
        expect(mocks.details.get('old')).toMatchObject({ name: 'Luna', desc: 'theirs' })
    })

    it('installs the import as new or not at all when the user says so', async () => {
        install(card('old', 'Luna'))
        const asNew = commitImportedCharacter(card('next', 'Luna v2'), commitCharacter)
        await opened()
        chooseContentUpdate('new')
        expect(await asNew).toBe('next')
        expect(mocks.details.get('old').name).toBe('Luna')

        platform.runtime = 'tauri'
        const assetsBefore = mocks.assets.size
        const cancelled = commitImportedCharacter(card('third', 'Luna v3', { image: 'assets/third.png' }), commitCharacter)
        await opened()
        chooseContentUpdate('cancel')
        expect(await cancelled).toBeNull()
        expect(mocks.details.has('third')).toBe(false)
        expect(mocks.assets.size).toBe(assetsBefore)
        // The card reader saved its assets already; they go when nothing uses them.
        expect(await releasedKeys()).toEqual(['assets/third.png'])
    })

    it('asks about the assets an earlier file does not carry, ones the user removed too', async () => {
        install(card('old', 'Luna', { desc: 'old', image: '', additionalAssets: [] }))
        const map = ['map', 'assets/map.png', 'png']
        const pending = commitImportedCharacter(card('next', 'Luna v2', { desc: 'new', image: 'assets/new.png', additionalAssets: [map] }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        const earlier = { spec: 'chara_card_v2', data: { name: 'Luna', description: 'old' } }
        vi.mocked(selectSingleFile).mockResolvedValueOnce({ name: 'Luna.json', data: new TextEncoder().encode(JSON.stringify(earlier)) })
        await loadContentUpdateBaseFile()
        expect(contentUpdateDialog.baseSource.kind).toBe('file')
        expect(contentUpdateDialog.plan!.items.map((item) => [item.field, item.status, item.choice])).toEqual([
            ['name', 'update', 'theirs'], ['desc', 'update', 'theirs'], ['image', 'differs', null], ['additionalAssets', 'differs', null],
        ])
        chooseContentUpdate('cancel')
        expect(await pending).toBeNull()
    })

    it('asks before low-level access the user turned on covers triggers the update takes', async () => {
        await commitImportedCharacter(card('first', 'Luna v1.0', { lowLevelAccess: false, triggerscript: [] }), commitCharacter)
        mocks.details.get('first').lowLevelAccess = true
        const trigger = { comment: 'lua', type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: 'x' }], lowLevelAccess: false }
        const update = async (id: string) => {
            // A card without the field, as older cards come.
            const pending = commitImportedCharacter(card(id, 'Luna v1.1', { triggerscript: [trigger] }), commitCharacter)
            await opened()
            await reviewContentUpdate()
            chooseContentUpdate('update')
            return await pending
        }
        mocks.alertConfirm.mockResolvedValueOnce(false)
        expect(await update('declined')).toBeNull()
        expect(mocks.details.has('declined')).toBe(false)
        expect(mocks.details.get('first').triggerscript).toEqual([])
        expect(await update('accepted')).toBe('first')
        expect(mocks.alertConfirm).toHaveBeenCalledTimes(2)
        expect(mocks.details.get('first')).toMatchObject({ lowLevelAccess: true, triggerscript: [trigger] })
    })

    it('opens the review of the named character for the update action, whatever the import is called', async () => {
        install(card('old', 'Luna', { desc: 'a' }))
        const result = importAsContentUpdate('character', 'old', () => commitImportedCharacter(card('next', 'Moon', { desc: 'b' }), commitCharacter))
        await reviewed()
        expect(contentUpdateDialog.target).toBe('old')
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
        chooseContentUpdate('update')
        expect(await result).toBe('old')
        expect(mocks.details.get('old')).toMatchObject({ name: 'Moon', desc: 'b' })
    })

    it('does not update a character while a request for it runs, streamed or not', async () => {
        install(card('old', 'Luna', { desc: 'a' }))
        const release = registerGeneratingConversation({ characterId: 'old', conversationId: 'old-chat' })
        try {
            const pending = commitImportedCharacter(card('next', 'Luna v2', { desc: 'b' }), commitCharacter)
            await opened()
            await reviewContentUpdate()
            expect(contentUpdateDialog.targetBusy).toBe(true)
            for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
            chooseContentUpdate('update')
            expect(contentUpdateDialog.request).not.toBeNull()
            release()
            chooseContentUpdate('update')
            expect(await pending).toBe('old')
        } finally {
            release()
        }
    })

    it.each([
        ['is gone', () => { mocks.missingTarget = true }, true],
        ['cannot be written', () => { mocks.failing.add('content-update') }, true],
        ['cannot be written and the import not restored', () => { mocks.failing.add('content-update').add('content-update-restore') }, false],
    ])('keeps the import as a new character only once it is back, when the installed one %s', async (_case, fail, restored) => {
        install(card('old', 'Luna', { desc: 'a' }))
        const pending = commitImportedCharacter(card('next', 'Luna v2', { desc: 'b' }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
        fail()
        chooseContentUpdate('update')
        expect(await pending).toBe(restored ? 'next' : null)
        expect(mocks.alertError).toHaveBeenCalledOnce()
        const text = language.risuNest.contentUpdate
        expect(mocks.alertError.mock.calls[0][0].split('\n')[0]).toBe(restored ? text.failed : text.notUpdated)
        const kept = mocks.details.get('next')
        if (restored) expect(kept).toMatchObject({ name: 'Luna v2', desc: 'b' })
        expect(kept.trashTime === undefined).toBe(restored)
        expect(mocks.root.characterOrder).toEqual(restored ? ['old', 'next'] : ['old'])
    })
})

describe('module update', () => {
    function imported(fileId: string, fields: Partial<RisuModule>) {
        const module = { id: fileId, description: '', ...fields } as RisuModule
        markModuleImport(module)
        module.id = `${fileId}-installed-${mocks.db.modules.length}`
        return module
    }

    it('replaces the installed module in place, keeping its id and enabled state', async () => {
        platform.runtime = 'tauri'
        await commitImportedModule(imported('file-1', { name: 'Lore', description: 'old', lorebook: [] }), commitModule)
        const installedId = mocks.db.modules[0].id
        const firstBase = mocks.db.modules[0].updateBase
        mocks.db.enabledModules.push(installedId)
        const pending = commitImportedModule(imported('file-1', { name: 'Renamed Lore', description: 'new', lorebook: [] }), commitModule)
        await opened()
        expect(contentUpdateDialog.request?.candidates.map((candidate) => [candidate.id, candidate.reason])).toEqual([[installedId, 'lineage']])
        await reviewContentUpdate()
        expect(contentUpdateDialog.plan!.items.map((item) => [item.field, item.choice])).toEqual([['description', 'theirs'], ['name', 'theirs']])
        chooseContentUpdate('update')
        expect(await pending).toBe(installedId)

        expect(mocks.db.modules).toHaveLength(1)
        expect(mocks.db.modules[0]).toMatchObject({ id: installedId, name: 'Renamed Lore', description: 'new', updateLineage: 'file-1' })
        expect(mocks.db.enabledModules).toEqual([installedId])
        const base = JSON.parse(new TextDecoder().decode(mocks.assets.get(mocks.db.modules[0].updateBase!)))
        expect(base).toEqual({ description: 'new', name: 'Renamed Lore', lorebook: [], updateLineage: 'file-1' })
        expect(await releasedKeys()).toContain(firstBase)
    })

    it('asks before low-level access the user turned on covers triggers from an import that leaves it out', async () => {
        await commitImportedModule(imported('file-3', { name: 'Lua Pack', trigger: [] }), commitModule)
        mocks.db.modules[0].lowLevelAccess = true
        const trigger = { comment: 'lua', type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: 'x' }], lowLevelAccess: false }
        mocks.alertConfirm.mockResolvedValueOnce(false)
        const pending = commitImportedModule(imported('file-3', { name: 'Lua Pack', trigger: [trigger] } as Partial<RisuModule>), commitModule)
        await opened()
        await reviewContentUpdate()
        chooseContentUpdate('update')
        expect(await pending).toBeNull()
        expect(mocks.alertConfirm).toHaveBeenCalledOnce()
        expect(mocks.db.modules).toHaveLength(1)
        expect(mocks.db.modules[0]).toMatchObject({ lowLevelAccess: true, trigger: [] })
    })

    it('installs the import as new when the installed module was removed before the update', async () => {
        mocks.db.modules.push({ id: 'gone', name: 'Lore', description: 'old' } as RisuModule)
        const incoming = imported('file-2', { name: 'Lore', description: 'new' })
        const pending = commitImportedModule(incoming, commitModule)
        await opened()
        await reviewContentUpdate()
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
        mocks.db.modules.splice(0, 1)
        chooseContentUpdate('update')
        expect(await pending).toBe(incoming.id)
        expect(mocks.db.modules.map((module) => module.id)).toEqual([incoming.id])
        expect(mocks.assets.get(incoming.updateBase!)).toBeDefined()
    })
})
