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
    /** Runs while the trashed copy of the installed character is written. */
    duringBackup: null as (() => void) | null,
    /** Runs while a lorebook is brought to the current format. */
    duringNormalize: null as (() => void) | null,
    /** Awaited before the installed character is read for a comparison. */
    beforeRead: null as (() => Promise<void>) | null,
    /** Awaited while an asset, such as the base of an import, is saved. */
    duringSave: null as (() => Promise<void>) | null,
}))
const platform = vi.hoisted(() => ({ runtime: 'web' as 'tauri' | 'web', os: 'windows' as 'windows' | 'android' }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({ runtime: platform.runtime, os: platform.os })))
const core = vi.hoisted(() => ({ invoke: vi.fn(async (_command: string, _args?: unknown): Promise<unknown> => ({ revision: 2 })) }))
vi.mock('@tauri-apps/api/core', async (importOriginal) => ({ ...await importOriginal<typeof import('@tauri-apps/api/core')>(), invoke: core.invoke }))

vi.mock('../globalApi.svelte', () => ({
    loadAsset: async (key: string) => mocks.assets.get(key) ?? null,
    saveAsset: async (data: Uint8Array) => {
        await mocks.duringSave?.()
        const key = `assets/base-${mocks.assets.size + 1}.json`
        mocks.assets.set(key, data)
        return key
    },
}))
// The alerts go through the app's queue, so a test reads what the screen shows.
vi.mock('../alert', async () => {
    const { alertStore } = await import('../alertQueue')
    return {
        alertClear: () => alertStore.clearStatus(),
        alertError: (msg: string) => {
            mocks.alertError(msg)
            alertStore.set({ type: 'error', msg })
        },
        alertConfirm: mocks.alertConfirm,
    }
})
vi.mock('../stores.svelte', async () => ({ DBState: { db: mocks.db }, alertStore: (await import('../alertQueue')).alertStore }))
vi.mock('../characters', () => ({
    updateLorebooks: (books: unknown[]) => {
        mocks.duringNormalize?.()
        return books
    },
}))
vi.mock('../process/inlayScreen', () => ({ updateInlayScreen: (value: unknown) => value }))
vi.mock('../util', () => ({ selectSingleFile: vi.fn(), fileAccept: () => '*' }))
// The pickers the apps use for an earlier file, which the native reader then reads by path.
vi.mock('../storage/fileSelection', () => ({ open: vi.fn() }))
vi.mock('../storage/nativeFileSelection', () => ({ withNativeSelectedPath: vi.fn() }))
// The earlier-file reader's lorebook converter, without the app state its module loads.
vi.mock('../characterCards', () => ({ convertCharbook: vi.fn() }))
vi.mock('../storage/database.svelte', () => ({ defaultSdDataFunc: () => [] }))
vi.mock('../storage/persistentDataRuntime.svelte', async () => ({
    assetAliasCleanupTarget: (await import('../storage/deletedOwnerAssetCleanup.testSupport')).createAssetAliasCleanupTarget,
    readPersistentCharacterDetail: async (id: string, reason: string) => {
        if (reason === 'content-update-read') await mocks.beforeRead?.()
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
        if (reason === 'content-update-backup') mocks.duringBackup?.()
        return true
    },
    mutateCharacterDetailFields: async (id: string, reason: string, mutate: (state: any) => void | Promise<void>) => {
        const detail = mocks.details.get(id)
        if (mocks.failing.has(reason)) throw new Error('write failed')
        if (!detail || mocks.missingTarget) return false
        const character = structuredClone(detail)
        await mutate({ character })
        mocks.details.set(id, editableDetail(detail, character))
        return true
    },
}))

import { get } from 'svelte/store'
import { language } from 'src/lang'
import { alertStore } from '../alertQueue'
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
import { open } from '../storage/fileSelection'
import { withNativeSelectedPath } from '../storage/nativeFileSelection'
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

/** The loading notice an import shows while it reads its file. */
const reading = { type: 'wait', msg: 'Loading... (Reading)' } as const

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
    mocks.duringBackup = null
    mocks.duringNormalize = null
    mocks.beforeRead = null
    mocks.duringSave = null
    mocks.alertError.mockClear()
    mocks.alertConfirm.mockReset().mockResolvedValue(true)
    alertStore.set({ type: 'none', msg: '' })
    alertStore.clearStatus()
    platform.runtime = 'web'
    platform.os = 'windows'
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
        const lore = (comment: string) => ({ comment, key: comment, content: '' })
        await commitImportedCharacter(card('first', 'Luna v1.0', { desc: 'old', personality: 'calm', backgroundHTML: '<b>', globalLore: [lore('shared'), lore('dropped')], extentions: { color: 'red' } }), commitCharacter)
        mocks.details.get('first').personality = 'brave'
        const firstBase = mocks.details.get('first').extentions[UPDATE_BASE_EXTENSION_KEY]
        // A stored import may not carry a type yet, unlike the installed card.
        const { type: _type, ...second } = card('second', 'Luna v1.1', { desc: 'new', personality: 'calm', backgroundHTML: undefined, globalLore: [lore('shared'), lore('dropped')], extentions: { color: 'red' } })
        alertStore.set(reading)
        const pending = commitImportedCharacter(second as any, commitCharacter)
        await opened()
        expect(get(alertStore).type).toBe('none')
        expect(contentUpdateDialog.request?.candidates.map((candidate) => candidate.id)).toEqual(['first'])
        await reviewContentUpdate()
        expect(unresolvedItems(contentUpdateDialog.plan!)).toEqual([])
        // While the old card is copied: a note saved, a lorebook entry and an extension value removed, an entry added.
        let shownWhileWritten: unknown
        mocks.duringBackup = () => {
            shownWhileWritten = get(alertStore)
            mocks.details.get('first').notes = 'written meanwhile'
            mocks.details.get('first').globalLore.splice(1, 1, lore('added meanwhile'))
            delete mocks.details.get('first').extentions.color
        }
        chooseContentUpdate('update')
        expect(await pending).toBe('first')
        // The import's loading notice covers the screen again until the update is written.
        expect(shownWhileWritten).toBe(reading)
        expect(get(alertStore).type).toBe('none')

        const updated = mocks.details.get('first')
        expect(updated).toMatchObject({ name: 'Luna v1.1', desc: 'new', personality: 'brave', notes: 'written meanwhile', chats: [{ id: 'first-chat' }] })
        expect(updated.globalLore).toEqual([lore('shared'), lore('added meanwhile')])
        expect(Object.keys(updated.extentions)).toEqual([UPDATE_BASE_EXTENSION_KEY])
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
        // A notice that is not a loading one does not come back after the dialog.
        alertStore.set({ type: 'toast', msg: 'saved' })
        let shownWhileStored: unknown
        commitCharacter.mockImplementationOnce(async (value) => {
            shownWhileStored = get(alertStore).type
            install(value)
        })
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
        expect(shownWhileStored).toBe('none')
        expect(mocks.details.get('old')).toMatchObject({ name: 'Luna', desc: 'theirs' })
    })

    it.each(['character', 'module'] as const)('takes the dialog down and fails with the reason when the import operation ends while it asks (%s)', async (kind) => {
        install(card('old', 'Luna'))
        mocks.db.modules.push({ id: 'installed-module', name: 'Lore', description: '', lorebook: [] })
        const operation = new AbortController()
        const stored = kind === 'character'
            ? commitImportedCharacter(card('next', 'Luna', { desc: 'new' }), commitCharacter, operation.signal)
            : commitImportedModule({ id: 'new-module', name: 'Lore', description: 'new', lorebook: [] } as RisuModule, commitModule, operation.signal)
        await opened()
        const expired = new Error('background time ended')
        operation.abort(expired)
        await expect(stored).rejects.toBe(expired)
        expect(contentUpdateDialog.request).toBeNull()
        expect(commitCharacter).not.toHaveBeenCalled()
        expect(commitModule).not.toHaveBeenCalled()
    })

    it.each(['character', 'module'] as const)('stores nothing when the import operation ends while the base is saved, and hands that base to the cleanup (%s)', async (kind) => {
        platform.runtime = 'tauri'
        const operation = new AbortController()
        const expired = new Error('background time ended')
        mocks.duringSave = async () => operation.abort(expired)
        const stored = kind === 'character'
            ? commitImportedCharacter(card('new', 'Luna'), commitCharacter, operation.signal)
            : commitImportedModule({ id: 'new-module', name: 'Lore', description: '', lorebook: [] } as RisuModule, commitModule, operation.signal)
        await expect(stored).rejects.toBe(expired)
        expect(commitCharacter).not.toHaveBeenCalled()
        expect(commitModule).not.toHaveBeenCalled()
        expect(mocks.assets.size).toBe(1)
        expect(await releasedKeys()).toEqual([...mocks.assets.keys()])
    })

    it.each(['character', 'module'] as const)('hands the base it saved to the cleanup when the import cannot be stored (%s)', async (kind) => {
        platform.runtime = 'tauri'
        const failed = new Error('import write failed')
        const stored = kind === 'character'
            ? commitImportedCharacter(card('new', 'Luna'), async () => { throw failed })
            : commitImportedModule({ id: 'new-module', name: 'Lore', description: '', lorebook: [] } as RisuModule, async () => { throw failed })
        await expect(stored).rejects.toBe(failed)
        expect(await releasedKeys()).toEqual([...mocks.assets.keys()])
    })

    it('compares and writes the creator and version where the character editor shows them', async () => {
        // The editor changed them in additionalData; the copies beside it stay as the import set them.
        install(card('old', 'Luna', { creator: 'Imported', characterVersion: '1.0', additionalData: { tag: [], creator: 'Edited', character_version: '1.0b' } }))
        const pending = commitImportedCharacter(card('next', 'Luna', { creator: 'Author', characterVersion: '2.0',
            additionalData: { tag: ['tag'], creator: 'Author', character_version: '2.0' } }), commitCharacter)
        await opened()
        expect(contentUpdateDialog.request?.candidates[0].creator).toBe('Edited')
        await reviewContentUpdate()
        expect(contentUpdateDialog.plan!.items.map((item) => [item.field, item.mine, item.theirs])).toEqual([
            ['creator', 'Edited', 'Author'], ['characterVersion', '1.0b', '2.0'],
        ])
        setContentUpdateChoice('creator', 'mine')
        setContentUpdateChoice('characterVersion', 'theirs')
        chooseContentUpdate('update')
        expect(await pending).toBe('old')
        expect(mocks.details.get('old')).toMatchObject({ creator: 'Edited', characterVersion: '2.0',
            additionalData: { tag: ['tag'], creator: 'Edited', character_version: '2.0' } })
    })

    it('installs the import as new or not at all when the user says so', async () => {
        install(card('old', 'Luna'))
        alertStore.set(reading)
        let shownWhileStored: unknown
        commitCharacter.mockImplementationOnce(async (value) => {
            shownWhileStored = get(alertStore)
            install(value)
        })
        const asNew = commitImportedCharacter(card('next', 'Luna v2'), commitCharacter)
        await opened()
        chooseContentUpdate('new')
        expect(await asNew).toBe('next')
        expect(shownWhileStored).toBe(reading)
        expect(mocks.details.get('old').name).toBe('Luna')

        platform.runtime = 'tauri'
        const assetsBefore = mocks.assets.size
        alertStore.set(reading)
        const cancelled = commitImportedCharacter(card('third', 'Luna v3', { image: 'assets/third.png' }), commitCharacter)
        await opened()
        chooseContentUpdate('cancel')
        expect(await cancelled).toBeNull()
        expect(get(alertStore).type).toBe('none')
        expect(mocks.details.has('third')).toBe(false)
        expect(mocks.assets.size).toBe(assetsBefore)
        // The card reader saved its assets already; they go when nothing uses them.
        expect(await releasedKeys()).toEqual(['assets/third.png'])
    })

    it.each(['review', 'earlier file'] as const)('leaves the dialog of a later import alone when the %s read for an earlier import ends', async (step) => {
        install(card('old', 'Luna', { desc: 'mine' }))
        const first = commitImportedCharacter(card('first', 'Luna', { desc: 'first' }), commitCharacter)
        await opened()
        if (step === 'earlier file') await reviewContentUpdate()
        let release!: () => void
        const held = new Promise<void>((resolve) => { release = resolve })
        if (step === 'review') mocks.beforeRead = () => held
        else vi.mocked(selectSingleFile).mockReturnValueOnce(held.then(() => ({ name: 'Luna.json', data: new TextEncoder().encode(JSON.stringify({ spec: 'chara_card_v2', data: { name: 'Luna', description: 'mine', extensions: {} } })) })))
        const stale = step === 'review' ? reviewContentUpdate() : loadContentUpdateBaseFile()
        // Another import asks while the read waits, which ends the first one.
        const second = commitImportedCharacter(card('second', 'Luna', { desc: 'second' }), commitCharacter)
        expect(await first).toBeNull()
        await vi.waitFor(() => expect(contentUpdateDialog.request?.incomingName).toBe('Luna'))
        expect(contentUpdateDialog.step).toBe('choose')
        mocks.beforeRead = null
        release()
        await stale
        expect(contentUpdateDialog).toMatchObject({ step: 'choose', plan: null, busy: false, baseSource: { kind: 'none' } })
        await reviewContentUpdate()
        expect(contentUpdateDialog.plan!.items.map((item) => [item.field, item.theirs])).toEqual([['desc', 'second']])
        chooseContentUpdate('cancel')
        expect(await second).toBeNull()
    })

    it('asks about the assets an earlier file does not carry, ones the user removed or reordered too', async () => {
        const asset = (name: string) => [name, `assets/${name}.png`, 'png']
        install(card('old', 'Luna', { desc: 'old', image: '', additionalAssets: [asset('b'), asset('a')] }))
        const pending = commitImportedCharacter(card('next', 'Luna v2', { desc: 'new', image: 'assets/new.png', additionalAssets: [asset('a'), asset('b'), asset('map')] }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        setContentUpdateChoice('additionalAssets', 'mine')
        const earlier = { spec: 'chara_card_v2', data: { name: 'Luna', description: 'old', extensions: {} } }
        vi.mocked(selectSingleFile).mockResolvedValueOnce({ name: 'Luna.json', data: new TextEncoder().encode(JSON.stringify(earlier)) })
        await loadContentUpdateBaseFile()
        expect(contentUpdateDialog.baseSource.kind).toBe('file')
        // A file the importers would refuse leaves that comparison as it is.
        const plan = contentUpdateDialog.plan
        vi.mocked(selectSingleFile).mockResolvedValueOnce({ name: 'Luna.json', data: new TextEncoder().encode(JSON.stringify({ spec: 'chara_card_v3', data: {} })) })
        await loadContentUpdateBaseFile()
        expect(contentUpdateDialog.plan).toBe(plan)
        expect(contentUpdateDialog.error).toBe(language.risuNest.contentUpdate.baseFileUnsupported)
        expect(contentUpdateDialog.plan!.items.map((item) => [item.field, !!item.order, item.status, item.choice])).toEqual([
            ['name', false, 'update', 'theirs'], ['desc', false, 'update', 'theirs'], ['image', false, 'differs', null],
            ['additionalAssets', true, 'differs', null], ['additionalAssets', false, 'differs', null],
        ])
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) if (!contentUpdateDialog.choices[item.id]) setContentUpdateChoice(item.id, 'theirs')
        chooseContentUpdate('update')
        expect(await pending).toBe('old')
        // The order picked before the file was read still holds.
        expect(mocks.details.get('old').additionalAssets.map((entry: string[]) => entry[0])).toEqual(['b', 'map', 'a'])
    })

    it('asks again about an entry another earlier file turns from merged into a conflict', async () => {
        const bold = (fields: Record<string, unknown>) => ({ comment: 'bold', type: 'editdisplay', in: '**', out: '<b>', ...fields })
        install(card('old', 'Luna', { customscript: [bold({ out: '<strong>' })] }))
        const pending = commitImportedCharacter(card('next', 'Luna', { customscript: [bold({ in: '**(.+?)**' })] }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        const earlier = (script: object) => ({ name: 'Luna.json', data: new TextEncoder().encode(JSON.stringify({ spec: 'chara_card_v2', data: { name: 'Luna', extensions: { risuai: { customScripts: [script] } } } })) })
        vi.mocked(selectSingleFile).mockResolvedValueOnce(earlier(bold({})))
        await loadContentUpdateBaseFile()
        const [item] = contentUpdateDialog.plan!.items
        expect(item).toMatchObject({ status: 'merged', choice: 'both' })
        setContentUpdateChoice(item.id, 'mine')
        setContentUpdateChoice(item.id, 'both')
        // In an earlier version the pattern was another one, which both sides changed.
        vi.mocked(selectSingleFile).mockResolvedValueOnce(earlier(bold({ in: '__' })))
        await loadContentUpdateBaseFile()
        expect(contentUpdateDialog.plan!.items).toMatchObject([{ id: item.id, status: 'conflict', choice: null }])
        expect(contentUpdateDialog.choices[item.id]).toBeUndefined()
        setContentUpdateChoice(item.id, 'both')
        expect(contentUpdateDialog.choices[item.id]).toBeUndefined()
        setContentUpdateChoice(item.id, 'theirs')
        chooseContentUpdate('update')
        expect(await pending).toBe('old')
        expect(mocks.details.get('old').customscript).toEqual([bold({ in: '**(.+?)**', out: '<strong>' })])
    })

    it.each([
        ['desktop', 'windows', 'C:\\Cards\\Luna.charx'],
        // The picked copy goes by the picker's name for it, which keeps the extension the reader is chosen by.
        ['Android', 'android', '/data/cache/selected_documents/1/source'],
    ] as const)('reads an earlier file on the %s app natively by its path', async (_label, os, path) => {
        platform.runtime = 'tauri'
        platform.os = os
        install(card('old', 'Luna', { desc: 'mine' }))
        const pending = commitImportedCharacter(card('next', 'Luna', { desc: 'new' }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        expect(contentUpdateDialog.plan!.items).toMatchObject([{ field: 'desc', status: 'differs' }])
        const webPicks = vi.mocked(selectSingleFile).mock.calls.length
        if (os === 'windows') vi.mocked(open).mockResolvedValueOnce(path)
        else vi.mocked(withNativeSelectedPath).mockImplementationOnce(async (_options, consume) => consume({ path, name: 'Luna.charx', bytes: 1, mimeType: '' }))
        const earlier = new TextEncoder().encode(JSON.stringify({ spec: 'chara_card_v3', data: { name: 'Luna', description: 'mine', extensions: {} } }))
        const reply = new Uint8Array(5 + earlier.length)
        reply[0] = 2
        new DataView(reply.buffer).setUint32(1, earlier.length, true)
        reply.set(earlier, 5)
        core.invoke.mockImplementation(async (command) => command === 'native_content_update_base_read' ? reply.buffer : { revision: 2 })
        try {
            await loadContentUpdateBaseFile()
        } finally {
            core.invoke.mockImplementation(async () => ({ revision: 2 }))
        }
        expect(core.invoke).toHaveBeenCalledWith('native_content_update_base_read', { path, format: 'charx' })
        expect(vi.mocked(selectSingleFile).mock.calls.length).toBe(webPicks)
        expect([contentUpdateDialog.error, contentUpdateDialog.busy, contentUpdateDialog.baseSource]).toEqual(['', false, { kind: 'file', name: 'Luna.charx' }])
        expect(contentUpdateDialog.plan!.items).toMatchObject([{ field: 'desc', status: 'update', choice: 'theirs' }])
        chooseContentUpdate('cancel')
        expect(await pending).toBeNull()
    })

    it('stores nothing and hands the assets of the import to the cleanup when the operation ends while low-level access is asked', async () => {
        await commitImportedCharacter(card('first', 'Luna v1.0', { lowLevelAccess: false, triggerscript: [] }), commitCharacter)
        mocks.details.get('first').lowLevelAccess = true
        platform.runtime = 'tauri'
        const operation = new AbortController()
        const expired = new Error('background time ended')
        mocks.alertConfirm.mockImplementationOnce(async () => {
            operation.abort(expired)
            return true
        })
        const trigger = { comment: 'lua', type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: 'x' }], lowLevelAccess: false }
        const pending = commitImportedCharacter(card('next', 'Luna v1.1', { image: 'assets/next.png', triggerscript: [trigger] }), commitCharacter, operation.signal)
        await opened()
        await reviewContentUpdate()
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
        chooseContentUpdate('update')
        await expect(pending).rejects.toBe(expired)
        expect(mocks.details.has('next')).toBe(false)
        expect(await releasedKeys()).toContain('assets/next.png')
    })

    it('asks before low-level access the user turned on covers triggers the update takes', async () => {
        await commitImportedCharacter(card('first', 'Luna v1.0', { lowLevelAccess: false, triggerscript: [] }), commitCharacter)
        mocks.details.get('first').lowLevelAccess = true
        const trigger = { comment: 'lua', type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: 'x' }], lowLevelAccess: false }
        const update = async (id: string, triggerscript = [trigger]) => {
            // A card without the field, as older cards come.
            const pending = commitImportedCharacter(card(id, 'Luna v1.1', { triggerscript }), commitCharacter)
            await opened()
            await reviewContentUpdate()
            for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
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
        // Taking the author's removal of that trigger brings in no code.
        expect(await update('removed', [])).toBe('first')
        expect(mocks.alertConfirm).toHaveBeenCalledTimes(2)
        expect(mocks.details.get('first').triggerscript).toEqual([])
        // Nor does taking the author's order of triggers the user reordered too.
        const [a, b, c] = ['a', 'b', 'c'].map((comment) => ({ ...trigger, comment }))
        expect(await update('three', [a, b, c])).toBe('first')
        mocks.details.get('first').triggerscript = [b, a, c]
        expect(await update('reordered', [a, c, b])).toBe('first')
        expect(mocks.alertConfirm).toHaveBeenCalledTimes(3)
        expect(mocks.details.get('first').triggerscript).toEqual([a, c, b])
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
        alertStore.set(reading)
        const pending = commitImportedCharacter(card('next', 'Luna v2', { desc: 'b' }), commitCharacter)
        await opened()
        await reviewContentUpdate()
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
        fail()
        chooseContentUpdate('update')
        expect(await pending).toBe(restored ? 'next' : null)
        expect(mocks.alertError).toHaveBeenCalledOnce()
        // The failure stays on screen when the loading notice goes.
        expect(get(alertStore).type).toBe('error')
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
        const loadingAssets = { type: 'progress', msg: 'Loading... (Loading Assets)', submsg: '50.00' } as const
        alertStore.set(loadingAssets)
        let shownWhileStored: unknown
        commitModule.mockImplementationOnce(async (value) => {
            shownWhileStored = get(alertStore)
            // A setting changed while the import is stored.
            mocks.db.modules[0].hideIcon = true
            mocks.db.modules.push(value)
            // A module order received while the merge is prepared puts a new list in place of this one.
            mocks.duringNormalize = () => {
                mocks.duringNormalize = null
                mocks.db.modules = [...mocks.db.modules].reverse()
            }
        })
        const pending = commitImportedModule(imported('file-1', { name: 'Renamed Lore', description: 'new', lorebook: [] }), commitModule)
        await opened()
        expect(contentUpdateDialog.request?.candidates.map((candidate) => [candidate.id, candidate.reason])).toEqual([[installedId, 'lineage']])
        await reviewContentUpdate()
        expect(contentUpdateDialog.plan!.items.map((item) => [item.field, item.choice])).toEqual([['description', 'theirs'], ['name', 'theirs']])
        chooseContentUpdate('update')
        expect(await pending).toBe(installedId)
        expect(shownWhileStored).toBe(loadingAssets)
        expect(get(alertStore).type).toBe('none')

        expect(mocks.db.modules).toHaveLength(1)
        expect(mocks.db.modules[0]).toMatchObject({ id: installedId, name: 'Renamed Lore', description: 'new', updateLineage: 'file-1', hideIcon: true })
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

    it('stores nothing and hands the assets of the import to the cleanup when the operation ends while low-level access is asked', async () => {
        await commitImportedModule(imported('file-3', { name: 'Lua Pack', trigger: [] }), commitModule)
        mocks.db.modules[0].lowLevelAccess = true
        platform.runtime = 'tauri'
        const operation = new AbortController()
        const expired = new Error('background time ended')
        mocks.alertConfirm.mockImplementationOnce(async () => {
            operation.abort(expired)
            return true
        })
        const trigger = { comment: 'lua', type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: 'x' }], lowLevelAccess: false }
        const pending = commitImportedModule(imported('file-3', { name: 'Lua Pack', trigger: [trigger], assets: [['icon', 'assets/next.png', 'png']] } as Partial<RisuModule>), commitModule, operation.signal)
        await opened()
        await reviewContentUpdate()
        for (const item of unresolvedItems(contentUpdateDialog.plan!)) setContentUpdateChoice(item.id, 'theirs')
        chooseContentUpdate('update')
        await expect(pending).rejects.toBe(expired)
        expect(mocks.db.modules).toHaveLength(1)
        expect(await releasedKeys()).toContain('assets/next.png')
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
