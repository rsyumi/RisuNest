import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    assets: new Map<string, Uint8Array>(),
    details: new Map<string, any>(),
    root: { characterOrder: [] as any[] },
    db: { characters: [] as any[], modules: [] as any[], enabledModules: [] as string[] },
    alertError: vi.fn(),
    missingTarget: false,
}))

vi.mock('../globalApi.svelte', () => ({
    loadAsset: async (key: string) => mocks.assets.get(key) ?? null,
    saveAsset: async (data: Uint8Array) => {
        const key = `assets/base-${mocks.assets.size + 1}.json`
        mocks.assets.set(key, data)
        return key
    },
}))
vi.mock('../alert', () => ({ alertClear: vi.fn(), alertError: mocks.alertError }))
vi.mock('../stores.svelte', () => ({ DBState: { db: mocks.db } }))
vi.mock('../characters', () => ({ updateLorebooks: (books: unknown[]) => books }))
vi.mock('../process/inlayScreen', () => ({ updateInlayScreen: (value: unknown) => value }))
vi.mock('../util', () => ({ selectSingleFile: vi.fn() }))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    readPersistentCharacterDetail: async (id: string) => {
        const detail = mocks.details.get(id)
        if (!detail) return null
        const { chats: _chats, ...rest } = detail
        return structuredClone(rest)
    },
    mutatePersistentCharacterDetail: async (id: string, _reason: string, mutate: (state: any) => void) => {
        const detail = mocks.details.get(id)
        if (!detail) return false
        mutate({ root: mocks.root, character: detail })
        return true
    },
    mutateCharacterDetailFields: async (id: string, _reason: string, mutate: (state: any) => void) => {
        const detail = mocks.details.get(id)
        if (!detail || mocks.missingTarget) return false
        mutate({ character: detail })
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
    reviewContentUpdate,
    setContentUpdateChoice,
} from './flow.svelte'
import { clearStreamingConversation, setStreamingConversation } from '../storage/streamingConversationRegistry'
import type { RisuModule } from '../process/modules'

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
    mocks.alertError.mockClear()
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
        await commitImportedCharacter(card('first', 'Luna v1.0', { desc: 'old', personality: 'calm' }), commitCharacter)
        mocks.details.get('first').personality = 'brave'
        const pending = commitImportedCharacter(card('second', 'Luna v1.1', { desc: 'new', personality: 'calm' }), commitCharacter)
        await opened()
        expect(contentUpdateDialog.request?.candidates.map((candidate) => candidate.id)).toEqual(['first'])
        await reviewContentUpdate()
        expect(unresolvedItems(contentUpdateDialog.plan!)).toEqual([])
        chooseContentUpdate('update')
        expect(await pending).toBe('first')

        const updated = mocks.details.get('first')
        expect(updated).toMatchObject({ name: 'Luna v1.1', desc: 'new', personality: 'brave', chats: [{ id: 'first-chat' }] })
        const backup = mocks.details.get('second')
        expect(backup).toMatchObject({ chaId: 'second', name: `Luna v1.0 ${language.risuNest.contentUpdate.backupSuffix}`, desc: 'old', personality: 'brave', chats: [{ id: 'second-chat' }] })
        expect(backup.trashTime).toEqual(expect.any(Number))
        expect(backup.extentions[UPDATE_BASE_EXTENSION_KEY]).toBeUndefined()
        expect(mocks.root.characterOrder).toEqual(['first'])
        const baseKey = updated.extentions[UPDATE_BASE_EXTENSION_KEY]
        expect(JSON.parse(new TextDecoder().decode(mocks.assets.get(baseKey)))).toMatchObject({ name: 'Luna v1.1', desc: 'new' })
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

        const cancelled = commitImportedCharacter(card('third', 'Luna v3'), commitCharacter)
        await opened()
        chooseContentUpdate('cancel')
        expect(await cancelled).toBeNull()
        expect(mocks.details.has('third')).toBe(false)
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

    it('does not update a character while it is generating a response', async () => {
        install(card('old', 'Luna', { desc: 'a' }))
        setStreamingConversation('old-chat', undefined)
        try {
            const pending = commitImportedCharacter(card('next', 'Luna v2', { desc: 'b' }), commitCharacter)
            await opened()
            await reviewContentUpdate()
            expect(contentUpdateDialog.targetBusy).toBe(true)
            for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
            chooseContentUpdate('update')
            expect(contentUpdateDialog.request).not.toBeNull()
            clearStreamingConversation('old-chat')
            chooseContentUpdate('update')
            expect(await pending).toBe('old')
        } finally {
            clearStreamingConversation('old-chat')
        }
    })

    it('keeps the import as a new character when the installed one is gone at the update', async () => {
        install(card('old', 'Luna', { desc: 'a' }))
        const pending = commitImportedCharacter(card('next', 'Luna v2', { desc: 'b' }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
        mocks.missingTarget = true
        chooseContentUpdate('update')
        expect(await pending).toBe('next')
        expect(mocks.alertError).toHaveBeenCalledOnce()
        const kept = mocks.details.get('next')
        expect(kept).toMatchObject({ name: 'Luna v2', desc: 'b' })
        expect(kept.trashTime).toBeUndefined()
        expect(mocks.root.characterOrder).toEqual(['old', 'next'])
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
        await commitImportedModule(imported('file-1', { name: 'Lore', description: 'old', lorebook: [] }), commitModule)
        const installedId = mocks.db.modules[0].id
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
    })
})
