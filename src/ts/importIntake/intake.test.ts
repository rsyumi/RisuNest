import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { get, writable, type Writable } from 'svelte/store'
import type { NativeImportIntakeClassification } from './classify'

const mocks = vi.hoisted(() => ({
    runtime: 'tauri' as 'tauri' | 'web',
    os: 'windows' as 'windows' | 'android',
    db: { characters: [] as Array<{ chaId: string; name: string }>, didFirstSetup: true },
    files: new Map<string, Uint8Array>(),
    removed: [] as string[],
    classified: new Map<string, unknown>(),
    classifyRequests: [] as Array<Array<{ source: unknown; displayName: string }>>,
    spools: new Map<string, Uint8Array>(),
    discarded: [] as string[],
    discardResult: true,
    writeHook: undefined as (() => void) | undefined,
    classifyHook: undefined as ((displayName: string) => void) | undefined,
    alertError: vi.fn(),
    characterPath: vi.fn(),
    characterProcess: vi.fn(),
    nativeCharacter: vi.fn(),
    nativeModule: vi.fn(),
    moduleData: vi.fn(),
    preset: vi.fn(),
    appendLorebook: vi.fn(),
    restore: vi.fn(),
    restoreAndroid: vi.fn(),
    importPlugin: vi.fn(async () => true),
}))

vi.mock('@tauri-apps/api/core', () => ({
    invoke: vi.fn(async (command: string, args?: any) => {
        if (command === 'app_paths_roots') return { data: '/data' }
        if (command === 'import_intake_classify') {
            const sources = args.request.sources as Array<{ source: unknown; displayName: string }>
            mocks.classifyRequests.push(sources)
            for (const { displayName } of sources) mocks.classifyHook?.(displayName)
            return sources.map(({ displayName }) => mocks.classified.get(displayName)
                ?? { kind: 'unsupported', refusal: 'unsupported-format' })
        }
        if (command === 'native_content_source_metadata') return mocks.spools.get(args.token)!.slice().buffer
        throw new Error(`unexpected ${command}`)
    }),
}))
vi.mock('@tauri-apps/api/path', () => ({ join: async (...parts: string[]) => parts.join('/') }))
vi.mock('@tauri-apps/plugin-fs', () => ({
    mkdir: vi.fn(async () => {}),
    readDir: vi.fn(async () => []),
    stat: vi.fn(),
    open: vi.fn(async (path: string) => {
        mocks.files.set(path, new Uint8Array())
        return {
            write: async (chunk: Uint8Array) => {
                mocks.writeHook?.()
                mocks.files.set(path, new Uint8Array([...mocks.files.get(path)!, ...chunk]))
                return chunk.length
            },
            close: async () => {},
        }
    }),
    readFile: vi.fn(async (path: string) => mocks.files.get(path)!),
    remove: vi.fn(async (path: string) => {
        mocks.removed.push(path)
        mocks.files.delete(path)
    }),
}))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({ runtime: mocks.runtime, os: mocks.os })))
vi.mock('../mobileBackgroundTask', async () => (await import('../mobileBackgroundTask.testSupport')).mobileBackgroundTaskModule())
vi.mock('../alert', () => ({ alertError: mocks.alertError, alertNormal: vi.fn() }))
vi.mock('../stores.svelte', () => ({
    DBState: { db: mocks.db },
    selectedCharID: writable(-1),
    settingsOpen: writable(false),
    SettingsMenuIndex: writable(0),
    loadedStore: writable(true),
    ShowRealmFrameStore: writable(''),
    popUpEditorStore: { open: false },
    irisStore: { open: false },
    onboardingModalStore: { open: false },
    customSideBarConfigDialogStore: { open: false },
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({ persistentWorkingSetInputBlocked: writable(false) }))
vi.mock('../storage/database.svelte', () => ({ getDatabase: () => mocks.db, importPreset: mocks.preset }))
vi.mock('../globalApi.svelte', () => ({ checkCharOrder: vi.fn() }))
vi.mock('../characterCards', () => ({
    importCharacterPathInOperation: mocks.characterPath,
    importCharacterProcess: mocks.characterProcess,
    importPreparedNativeCharacterContent: mocks.nativeCharacter,
    showRealmInfoStore: writable(null),
}))
vi.mock('../process/modules', () => ({
    importModuleData: mocks.moduleData,
    importPreparedNativeModuleContent: mocks.nativeModule,
}))
vi.mock('../process/lorebook.svelte', () => ({
    lorebookFileEntries: (imported: { data: unknown[] }) => imported.data,
    appendCharacterLorebook: mocks.appendLorebook,
}))
vi.mock('../plugins/plugins.svelte', () => ({ importPlugin: mocks.importPlugin }))
vi.mock('../storage/portableBackupFileRouteProduction.svelte', () => ({ restoreBackupFromNativeSource: mocks.restore }))
vi.mock('../storage/androidRisuSaveRouteProduction.svelte', () => ({
    restoreAndroidOpenedBackupSource: mocks.restoreAndroid,
    showAndroidFileError: vi.fn(),
}))

import { language } from 'src/lang'
import { onboardingHold } from 'src/lib/Others/Onboarding/onboardingGate'
import { alertStore } from '../alertQueue'
import { setAppLockInputBlocked } from '../appLock/input'
import { RISU_APP_INTERNAL_DRAG_TYPE } from '../dragTypes'
import { failureReason } from '../gui/nativeFileJobDialogModel'
import { closeTextEditorPopup, openTextEditorPopup, type TextEditorPopupRequest } from '../gui/textEditorPopup.svelte'
import {
    cancelActiveNativeFileOperation,
    dismissNativeFileOperationOutcome,
    nativeFileOperation,
    nativeFileOperationOutcome,
    NativeFileOperationBusyError,
    runSharedNativeFileOperation,
    type NativeFileOperationOutcome,
} from '../storage/nativeFileJobManager'
import { MAX_CONTENT_METADATA_BYTES } from '../storage/contentImportLimits'
import { NativeFileJobError } from '../storage/nativeFileJobs'
import { persistentWorkingSetInputBlocked } from '../storage/persistentDataRuntime.svelte'
import { loadedStore, selectedCharID, settingsOpen, SettingsMenuIndex } from '../stores.svelte'
import { appUpdateState } from '../update/state.svelte'
import { handleAppDrop } from './drop'
import { pluginInstallDialog } from './pluginInstall'
import { installImportIntake, submitImportDelivery } from './intake'
import { importDropOverlayVisible, importSheet, resolveImportSheet } from './sheetStore'
import type { ImportIntakeDelivery, ImportSheetDecision, ImportSheetRequest } from './types'

// The mocked store above is writable.
const workingSetBlocked = persistentWorkingSetInputBlocked as Writable<boolean>
const showUpdatePopup = (popupVisible: boolean) => appUpdateState.update(state => ({ ...state, popupVisible }))
const editorRequest: TextEditorPopupRequest = { value: 'synthetic', save: () => true }

const encode = (text: string) => new TextEncoder().encode(text)
const dropped = (name: string, content = `synthetic ${name}`) => new File([encode(content)], name)
const drop = (...files: File[]): ImportIntakeDelivery => ({ origin: 'drop', sources: files.map(file => ({ type: 'file', file })) })
const classify = (name: string, result: NativeImportIntakeClassification) => mocks.classified.set(name, result)
const inboxCopies = () => [...mocks.files.keys()].filter(path => path.startsWith('/data/content-import-inbox/'))

let outcomes: NativeFileOperationOutcome[] = []
let operationsOpened = 0
let unsubscribe: (() => void)[] = []

async function answerSheet(decide: (request: ImportSheetRequest) => ImportSheetDecision): Promise<ImportSheetRequest> {
    await vi.waitFor(() => expect(get(importSheet)).not.toBeNull())
    const { request } = get(importSheet)!
    resolveImportSheet(decide(request))
    return request
}

beforeEach(() => {
    mocks.runtime = 'tauri'
    mocks.os = 'windows'
    mocks.db.characters = []
    mocks.db.didFirstSetup = true
    mocks.files.clear()
    mocks.removed.length = 0
    mocks.classified.clear()
    mocks.classifyRequests.length = 0
    mocks.classifyHook = undefined
    mocks.spools.clear()
    mocks.discarded.length = 0
    mocks.discardResult = true
    mocks.writeHook = undefined
    vi.clearAllMocks()
    selectedCharID.set(-1)
    settingsOpen.set(false)
    SettingsMenuIndex.set(0)
    dismissNativeFileOperationOutcome()
    Object.assign(window, { RisuSafBridge: { discardSource: (token: string) => {
        mocks.discarded.push(token)
        return mocks.discardResult
    } } })
    mocks.characterPath.mockImplementation(async (_path, context, name: string) => {
        context.setLocation({ kind: 'character', chaId: `${name}-id` })
        return `${name}-id`
    })
    mocks.characterProcess.mockImplementation(async ({ name }: { name: string }) => {
        mocks.db.characters.push({ chaId: `${name}-id`, name })
        return mocks.db.characters.length - 1
    })
    mocks.nativeModule.mockResolvedValue({ kind: 'imported', value: 'native-module' })
    mocks.moduleData.mockResolvedValue({ kind: 'module-from-lorebook' })
    mocks.preset.mockResolvedValue(3)
    mocks.appendLorebook.mockResolvedValue(true)
    mocks.restore.mockResolvedValue(null)
    outcomes = []
    operationsOpened = 0
    let open = false
    unsubscribe = [
        nativeFileOperationOutcome.subscribe(outcome => { if (outcome) outcomes.push(outcome) }),
        nativeFileOperation.subscribe(state => {
            if (state && !open) operationsOpened++
            open = !!state
        }),
    ]
})

afterEach(() => {
    for (const stop of unsubscribe) stop()
    resolveImportSheet(null)
})

describe('one dropped file', () => {
    it('imports a character from its inbox copy in the dialog that copied it, then removes the copy', async () => {
        classify('card.png', { kind: 'character', format: 'png' })
        const file = dropped('card.png')
        mocks.characterPath.mockImplementation(async (path: string, context, name: string) => {
            expect(mocks.files.get(path)).toEqual(encode('synthetic card.png'))
            context.setLocation({ kind: 'character', chaId: `${name}-id` })
            return `${name}-id`
        })

        await submitImportDelivery(drop(file))

        expect(mocks.characterPath).toHaveBeenCalledExactlyOnceWith(
            expect.stringMatching(/^\/data\/content-import-inbox\/.+\.png$/), expect.anything(), 'card.png')
        expect(outcomes).toEqual([expect.objectContaining({
            state: 'succeeded', source: { name: 'card.png', bytes: file.size },
            location: { kind: 'character', chaId: 'card.png-id' },
        })])
        expect(operationsOpened).toBe(1)
        expect(get(importSheet)).toBeNull()
        expect(inboxCopies()).toEqual([])
    })

    it.each([
        ['book.risum', { kind: 'module', format: 'risum' }, { kind: 'module' }],
        ['card.charx', { kind: 'character', format: 'charx' }, { kind: 'character', chaId: 'card.charx-id' }],
        ['preset.RISUP', { kind: 'preset', format: 'risup' }, { kind: 'preset' }],
        ['book.json', { kind: 'lorebook', format: 'json' }, { kind: 'module-from-lorebook' }],
    ] as const)('imports %s directly and records where it went', async (name, result, location) => {
        classify(name, result)
        await submitImportDelivery(drop(dropped(name, '{"type":"risu","data":[]}')))
        expect(outcomes).toEqual([expect.objectContaining({ state: 'succeeded', location })])
        expect(get(importSheet)).toBeNull()
        if (result.kind === 'preset') {
            expect(mocks.preset).toHaveBeenCalledExactlyOnceWith({ name: 'preset.risup', data: encode('{"type":"risu","data":[]}') })
        }
    })

    it('imports a dropped .preset file, which the preset picker also accepts', async () => {
        classify('legacy.preset', { kind: 'preset', format: 'json' })
        await submitImportDelivery(drop(dropped('legacy.preset', '{"temperature":1}')))
        expect(mocks.preset).toHaveBeenCalledExactlyOnceWith({ name: expect.stringMatching(/^legacy\.preset/), data: encode('{"temperature":1}') })
        expect(outcomes).toEqual([expect.objectContaining({ state: 'succeeded', location: { kind: 'preset' } })])
    })

    it.each([
        ['card.risudat', 'png', 'card.risudat.png'],
        ['card.bin', 'jpeg-charx', 'card.bin.jpeg'],
        ['card.JPG', 'jpeg-charx', 'card.JPG'],
    ] as const)('hands %s to the importer under a name of the format its content shows', async (name, format, importName) => {
        classify(name, { kind: 'character', format })
        await submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path: `C:/opened/${name}` }] })
        expect(mocks.characterPath).toHaveBeenCalledExactlyOnceWith(`C:/opened/${name}`, expect.anything(), importName)
    })

    it('alerts the name and the reason of a file it cannot import', async () => {
        classify('photo.jpg', { kind: 'unsupported', refusal: 'plain-jpeg' })
        classify('huge.png', { kind: 'unsupported', refusal: 'png-without-card' })
        classify('shared.json', { kind: 'unsupported', refusal: 'too-large' })
        const hugeJson = dropped('huge.json', '\n  {"name":"synthetic"}')
        // Whitespace past the bytes the check reads still counts as JSON.
        const paddedJson = dropped('padded.json', `${' '.repeat(64 * 1024)}{"name":"synthetic"}`)
        const hugePng = dropped('huge.png', 'synthetic image')
        const hugePlugin = dropped('huge.js', '//@name synthetic_huge\n//@api 3.0\n')
        for (const file of [hugeJson, paddedJson, hugePng, hugePlugin]) Object.defineProperty(file, 'size', { value: MAX_CONTENT_METADATA_BYTES + 1 })
        const copyHugeJson = vi.spyOn(hugeJson, 'stream')
        const copyPaddedJson = vi.spyOn(paddedJson, 'stream')
        const readHugePlugin = vi.spyOn(hugePlugin, 'text')
        await submitImportDelivery(drop(dropped('photo.jpg')))
        await submitImportDelivery(drop(dropped('notes.txt')))
        await submitImportDelivery(drop(hugeJson))
        await submitImportDelivery(drop(paddedJson))
        await submitImportDelivery(drop(hugePng))
        await submitImportDelivery(drop(hugePlugin))
        await submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path: '/opened/shared.json' }] })
        expect(mocks.alertError.mock.calls).toEqual([
            [`photo.jpg\n${language.risuNest.importDialog.reasonPlainJpeg}`],
            [`notes.txt\n${language.risuNest.importDialog.reasonUnsupportedFormat}`],
            [`huge.json\n${failureReason('payload-too-large', 'import', 'content')}`],
            [`padded.json\n${failureReason('payload-too-large', 'import', 'content')}`],
            [`huge.png\n${language.risuNest.importIntake.reasonPngWithoutCard}`],
            [`huge.js\n${failureReason('payload-too-large', 'import', 'content')}`],
            [`shared.json\n${failureReason('payload-too-large', 'import', 'content')}`],
        ])
        // A dropped JSON file over the limit is refused before it is copied; another large file is copied.
        expect(copyHugeJson).not.toHaveBeenCalled()
        expect(copyPaddedJson).not.toHaveBeenCalled()
        expect(readHugePlugin).not.toHaveBeenCalled()
        expect(get(pluginInstallDialog)).toBeNull()
        // A file of an unknown extension is never copied or read.
        expect(mocks.classifyRequests.flat().map(({ displayName }) => displayName)).toEqual(['photo.jpg', 'huge.png', 'shared.json'])
        expect(outcomes).toEqual([])
        expect(inboxCopies()).toEqual([])
    })

    it('ends the delivery when the copy is cancelled', async () => {
        mocks.writeHook = () => cancelActiveNativeFileOperation()
        await submitImportDelivery(drop(dropped('card.png')))
        expect(mocks.classifyRequests).toEqual([])
        expect(mocks.characterPath).not.toHaveBeenCalled()
        expect(outcomes).toEqual([expect.objectContaining({ state: 'cancelled' })])
        expect(inboxCopies()).toEqual([])
    })

    it('reads no further file once the delivery is cancelled during a classification', async () => {
        classify('first.png', { kind: 'character', format: 'png' })
        classify('second.png', { kind: 'character', format: 'png' })
        const second = dropped('second.png')
        const copySecond = vi.spyOn(second, 'stream')
        mocks.classifyHook = () => cancelActiveNativeFileOperation()
        await submitImportDelivery(drop(dropped('first.png'), second))
        expect(mocks.classifyRequests.flat().map(({ displayName }) => displayName)).toEqual(['first.png'])
        expect(copySecond).not.toHaveBeenCalled()
        expect(get(importSheet)).toBeNull()
        expect(outcomes).toEqual([expect.objectContaining({ state: 'cancelled' })])
        expect(inboxCopies()).toEqual([])
    })
})

describe('plugin files', () => {
    const pluginCode = ['//@name synthetic_helper', '//@display-name Synthetic Helper', '//@api 3.0', ''].join('\n')

    it('installs a dropped plugin once the user confirms it in the plugin dialog', async () => {
        const delivered = submitImportDelivery(drop(dropped('helper.js', pluginCode)))
        await vi.waitFor(() => expect(get(pluginInstallDialog)).not.toBeNull())
        const active = get(pluginInstallDialog)!
        expect(active.prompt).toEqual(expect.objectContaining({
            fileName: 'helper.js',
            header: expect.objectContaining({ name: 'synthetic_helper', displayName: 'Synthetic Helper' }),
        }))
        await active.install()
        await delivered
        expect(mocks.importPlugin).toHaveBeenCalledExactlyOnceWith(pluginCode, { isTypescript: false, confirmed: true })
        expect(mocks.classifyRequests).toEqual([])
        expect(get(pluginInstallDialog)).toBeNull()
    })

    it('refuses a plugin for another API version, and a plugin file that was opened or shared', async () => {
        classify('shared.js', { kind: 'script', format: 'js' })
        await submitImportDelivery(drop(dropped('old.js', ['//@name old_helper', '//@api 2.1', ''].join('\n'))))
        await submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path: '/opened/shared.js' }] })
        expect(mocks.alertError.mock.calls).toEqual([
            [`old.js\n${language.risuNest.plugins.unsupportedApiVersionInstall.replace('{version}', '2.1')}`],
            [`shared.js\n${language.risuNest.importDialog.reasonUnsupportedFormat}`],
        ])
        expect(get(pluginInstallDialog)).toBeNull()
        expect(mocks.importPlugin).not.toHaveBeenCalled()
    })
})

describe('destination choices', () => {
    it('asks where a lorebook goes when a character is selected, and appends it to that character', async () => {
        mocks.db.characters = [{ chaId: 'alice', name: 'Alice' }]
        selectedCharID.set(0)
        classify('book.json', { kind: 'lorebook', format: 'json' })
        const delivered = submitImportDelivery(drop(dropped('book.json', '{"type":"risu","data":[{"key":"synthetic"}]}')))
        const request = await answerSheet(({ items, defaults }) => [{ id: items[0].id, ...defaults[items[0].id] }])
        await delivered

        expect(request.lorebookCharacter).toEqual({ chaId: 'alice', name: 'Alice' })
        expect(request.defaults[request.items[0].id]).toEqual({ lorebookTarget: { type: 'character', chaId: 'alice', name: 'Alice' } })
        expect(mocks.appendLorebook).toHaveBeenCalledExactlyOnceWith('alice', [{ key: 'synthetic' }])
        expect(mocks.moduleData).not.toHaveBeenCalled()
        expect(outcomes).toEqual([expect.objectContaining({
            state: 'succeeded', location: { kind: 'character-lorebook', chaId: 'alice', characterName: 'Alice' },
        })])
        expect(inboxCopies()).toEqual([])
    })

    it('reports an empty lorebook as imported into the character and a character removed meanwhile as failed', async () => {
        mocks.db.characters = [{ chaId: 'alice', name: 'Alice' }]
        selectedCharID.set(0)
        classify('empty.json', { kind: 'lorebook', format: 'json' })
        classify('book.json', { kind: 'lorebook', format: 'json' })
        const toCharacter = ({ items, defaults }: ImportSheetRequest) => [{ id: items[0].id, ...defaults[items[0].id] }]
        let delivered = submitImportDelivery(drop(dropped('empty.json', '{"type":"risu","data":[]}')))
        await answerSheet(toCharacter)
        await delivered
        expect(outcomes).toEqual([expect.objectContaining({
            state: 'succeeded', location: { kind: 'character-lorebook', chaId: 'alice', characterName: 'Alice' },
        })])

        dismissNativeFileOperationOutcome()
        mocks.appendLorebook.mockResolvedValueOnce(false)
        delivered = submitImportDelivery(drop(dropped('book.json', '{"type":"risu","data":[{"key":"synthetic"}]}')))
        await answerSheet(toCharacter)
        await delivered
        expect(outcomes.at(-1)).toEqual(expect.objectContaining({ state: 'failed', error: expect.objectContaining({ code: 'revision-conflict' }) }))
    })

    it.each([
        ['nothing', undefined],
        ['the playground', { chaId: '§playground', name: 'Playground' }],
    ])('imports a lone lorebook into a new module without asking while %s is selected', async (_name, character) => {
        mocks.db.characters = character ? [character] : []
        selectedCharID.set(character ? 0 : -1)
        classify('book.json', { kind: 'lorebook', format: 'json' })
        await submitImportDelivery(drop(dropped('book.json', '{"type":"risu","data":[{"key":"synthetic"}]}')))

        expect(get(importSheet)).toBeNull()
        expect(mocks.moduleData).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ signal: expect.any(AbortSignal) }))
        expect(mocks.appendLorebook).not.toHaveBeenCalled()
    })

    it('points CharX and lorebook defaults at modules for a module name or a drop on the module settings page', async () => {
        mocks.db.characters = [{ chaId: 'alice', name: 'Alice' }]
        selectedCharID.set(0)
        classify('pack.module.charx', { kind: 'character', format: 'charx' })
        classify('card.charx', { kind: 'character', format: 'charx' })
        classify('book.json', { kind: 'lorebook', format: 'json' })

        let delivered = submitImportDelivery(drop(dropped('pack.module.charx'), dropped('card.charx'), dropped('book.json')))
        let request = await answerSheet(() => null)
        await delivered
        expect(Object.values(request.defaults)).toEqual([
            { charxAs: 'module' }, { charxAs: 'character' },
            { lorebookTarget: { type: 'character', chaId: 'alice', name: 'Alice' } },
        ])

        settingsOpen.set(true)
        SettingsMenuIndex.set(14)
        delivered = submitImportDelivery(drop(dropped('card.charx'), dropped('book.json')))
        request = await answerSheet(() => null)
        await delivered
        expect(Object.values(request.defaults)).toEqual([{ charxAs: 'module' }, { lorebookTarget: { type: 'new-module' } }])

        // Opened files follow the name only.
        delivered = submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path: 'C:/opened/card.charx' }] })
        await delivered
        expect(mocks.characterPath).toHaveBeenCalledExactlyOnceWith('C:/opened/card.charx', expect.anything(), 'card.charx')
        expect(mocks.nativeModule).not.toHaveBeenCalled()
        expect(inboxCopies()).toEqual([])
    })

    it('imports a CharX chosen as a module through the module import', async () => {
        classify('pack.module.charx', { kind: 'character', format: 'charx' })
        const delivered = submitImportDelivery(drop(dropped('pack.module.charx')))
        await answerSheet(({ items, defaults }) => [{ id: items[0].id, ...defaults[items[0].id] }])
        await delivered
        expect(mocks.nativeModule).toHaveBeenCalledOnce()
        expect(mocks.characterPath).not.toHaveBeenCalled()
        expect(outcomes).toEqual([expect.objectContaining({ state: 'succeeded', location: { kind: 'module-from-charx' } })])
    })
})

describe('several files', () => {
    it('refuses a backup among other files, imports only the checked rows and lists how each ended', async () => {
        classify('card.png', { kind: 'character', format: 'png' })
        classify('backup.risunest', { kind: 'backup', format: 'portable' })
        classify('photo.jpg', { kind: 'unsupported', refusal: 'plain-jpeg' })
        classify('book.risum', { kind: 'module', format: 'risum' })
        classify('other.png', { kind: 'character', format: 'png' })
        classify('preset.risup', { kind: 'preset', format: 'risup' })
        const failure = new Error('synthetic module failure')
        mocks.nativeModule.mockRejectedValue(failure)
        mocks.characterPath.mockImplementation(async (_path, context, name: string) => {
            if (name === 'other.png') return null
            context.setLocation({ kind: 'character', chaId: `${name}-id` })
            return `${name}-id`
        })

        const delivered = submitImportDelivery(drop(...['card.png', 'backup.risunest', 'notes.txt', 'photo.jpg', 'book.risum', 'other.png', 'preset.risup'].map(name => dropped(name))))
        const request = await answerSheet(({ items }) => items
            .filter(item => !item.refusal && item.name !== 'preset.risup')
            .map(item => ({ id: item.id })))
        expect(get(nativeFileOperation)).toBeNull()
        await delivered

        expect(request.items.map(item => [item.name, item.kind, item.refusal?.code])).toEqual([
            ['card.png', 'character', undefined],
            ['backup.risunest', 'backup', 'backup-with-other-files'],
            ['notes.txt', 'unsupported', 'unsupported-format'],
            ['photo.jpg', 'unsupported', 'plain-jpeg'],
            ['book.risum', 'module', undefined],
            ['other.png', 'character', undefined],
            ['preset.risup', 'preset', undefined],
        ])
        expect(mocks.restore).not.toHaveBeenCalled()
        expect(mocks.preset).not.toHaveBeenCalled()
        expect(outcomes).toEqual([expect.objectContaining({
            state: 'failed',
            batch: expect.objectContaining({ total: 3, imported: 1, declined: ['other.png'], entries: [
                { name: 'card.png', state: 'imported', location: { kind: 'character', chaId: 'card.png-id' } },
                { name: 'book.risum', state: 'failed', error: expect.objectContaining({ message: failure.message }) },
                { name: 'other.png', state: 'declined' },
            ] }),
        })])
        expect(inboxCopies()).toEqual([])
    })

    it('imports nothing and removes every copy when the sheet is cancelled', async () => {
        classify('first.png', { kind: 'character', format: 'png' })
        classify('second.png', { kind: 'character', format: 'png' })
        const delivered = submitImportDelivery(drop(dropped('first.png'), dropped('second.png')))
        await answerSheet(() => null)
        await delivered
        expect(mocks.characterPath).not.toHaveBeenCalled()
        expect(outcomes).toEqual([])
        expect(mocks.removed).toHaveLength(2)
        expect(inboxCopies()).toEqual([])
    })
})

describe('backups', () => {
    it('restores a lone dropped backup from its copy under the dropped name and removes the copy once the outcome is gone', async () => {
        classify('backup.risunest', { kind: 'backup', format: 'portable' })
        const file = dropped('backup.risunest')
        let copy = ''
        mocks.restore.mockImplementation(async (source: { path: string }) => {
            copy = source.path
            return await runSharedNativeFileOperation('import', 'backup-import', async () => ({ revision: 1 }), { presentation: 'dialog' })
        })

        let settled = false
        const delivered = submitImportDelivery(drop(file)).then(() => { settled = true })
        await vi.waitFor(() => expect(outcomes).toHaveLength(1))
        expect(mocks.restore).toHaveBeenCalledExactlyOnceWith(
            { type: 'desktopPath', path: copy },
            { sourceInfo: { name: 'backup.risunest', bytes: file.size } },
        )
        await new Promise(resolve => setTimeout(resolve, 0))
        expect(settled).toBe(false)
        expect(mocks.files.has(copy)).toBe(true)

        dismissNativeFileOperationOutcome()
        await delivered
        expect(mocks.removed).toEqual([copy])
    })

    it('restores a backup once an operation that started before it has ended', async () => {
        classify('backup.risunest', { kind: 'backup', format: 'portable' })
        let finishOther: (() => void) | undefined
        mocks.restore.mockImplementationOnce(async () => {
            // Another file operation started between the idle check and the restore.
            void runSharedNativeFileOperation('import', 'other', () => new Promise<string>(resolve => { finishOther = () => resolve('done') }), { presentation: 'dialog' })
            throw new NativeFileOperationBusyError()
        })
        const delivered = submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path: 'C:/opened/backup.risunest' }] })
        await vi.waitFor(() => expect(finishOther).toBeDefined())
        expect(mocks.restore).toHaveBeenCalledOnce()

        finishOther!()
        await vi.waitFor(() => expect(get(nativeFileOperationOutcome)).not.toBeNull())
        dismissNativeFileOperationOutcome()
        await delivered
        expect(mocks.restore).toHaveBeenCalledTimes(2)
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('restores a lone opened backup from its path and keeps the file', async () => {
        classify('backup.risunest', { kind: 'backup', format: 'portable', bytes: 4096 })
        await submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path: 'C:/opened/backup.risunest' }] })
        expect(mocks.restore).toHaveBeenCalledExactlyOnceWith(
            { type: 'desktopPath', path: 'C:/opened/backup.risunest' },
            { sourceInfo: { name: 'backup.risunest', bytes: 4096 } },
        )
        expect(mocks.removed).toEqual([])
    })
})

describe('holding', () => {
    it.each([
        ['an alert dialog', async () => {
            const dialog = alertStore.open({ type: 'normal', msg: 'synthetic' })
            return async () => {
                alertStore.set({ type: 'none', msg: '' })
                await dialog
            }
        }],
        ['a file operation outcome', async () => {
            await runSharedNativeFileOperation('import', 'synthetic', async () => 'done', { presentation: 'dialog' })
            expect(get(nativeFileOperationOutcome)).not.toBeNull()
            return async () => dismissNativeFileOperationOutcome()
        }],
        ['the app lock', async () => {
            setAppLockInputBlocked(true, false)
            return async () => setAppLockInputBlocked(false, false)
        }],
        ['loading', async () => {
            loadedStore.set(false)
            return async () => loadedStore.set(true)
        }],
        ['the library recovery screen', async () => {
            workingSetBlocked.set(true)
            return async () => workingSetBlocked.set(false)
        }],
        ['the update popup', async () => {
            showUpdatePopup(true)
            return async () => showUpdatePopup(false)
        }],
        ['a full-screen editor', async () => {
            openTextEditorPopup(editorRequest)
            return async () => closeTextEditorPopup(editorRequest)
        }],
    ])('waits for %s before it starts', async (_, block) => {
        classify('card.png', { kind: 'character', format: 'png' })
        const unblock = await block()
        const outcome = get(nativeFileOperationOutcome)

        const delivered = submitImportDelivery(drop(dropped('card.png')))
        await new Promise(resolve => setTimeout(resolve, 0))
        expect(get(nativeFileOperation)).toBeNull()
        expect(get(nativeFileOperationOutcome)).toBe(outcome)
        expect(mocks.characterPath).not.toHaveBeenCalled()

        await unblock()
        await delivered
        expect(mocks.characterPath).toHaveBeenCalledOnce()
    })

    it('handles a second delivery after the first one ended', async () => {
        classify('first.png', { kind: 'character', format: 'png' })
        classify('second.png', { kind: 'character', format: 'png' })
        classify('third.png', { kind: 'character', format: 'png' })
        const first = submitImportDelivery(drop(dropped('first.png'), dropped('second.png')))
        const second = submitImportDelivery(drop(dropped('third.png')))
        await answerSheet(() => {
            expect(mocks.classifyRequests.flat().map(({ displayName }) => displayName)).toEqual(['first.png', 'second.png'])
            return null
        })
        await first
        await second
        expect(mocks.characterPath).toHaveBeenCalledExactlyOnceWith(expect.any(String), expect.anything(), 'third.png')
    })
})

describe('Android shares', () => {
    const spool = (token: string, displayName: string, content = '{}') => {
        mocks.spools.set(token, encode(content))
        return { type: 'androidSpool' as const, spool: { token, displayName, bytes: content.length } }
    }

    beforeEach(() => {
        mocks.os = 'android'
    })

    it('refuses and releases a shared backup that came with other files and imports the rest', async () => {
        classify('backup.risunest', { kind: 'backup', format: 'portable' })
        classify('card.json', { kind: 'character', format: 'json' })
        classify('preset.risup', { kind: 'preset', format: 'risup' })
        mocks.discardResult = false
        // A share delivers its spooled files and the cache copies of the other files together.
        const presetPath = `/data/cache/opened_files/${'b'.repeat(64)}-1-preset.risup`
        mocks.files.set(presetPath, encode('synthetic preset'))
        const delivered = submitImportDelivery({ origin: 'opened', sources: [
            spool('backup-token', 'backup.risunest'),
            spool('card-token', 'card.json', '{"name":"Synthetic"}'),
            { type: 'path', path: presetPath },
            { type: 'failed', name: 'broken.png', code: 'source-copy-failed' },
        ] })
        const request = await answerSheet(({ items }) => items.filter(item => !item.refusal).map(item => ({ id: item.id })))
        await delivered

        expect(request.items.map(item => [item.name, item.refusal?.code])).toEqual([
            ['backup.risunest', 'backup-with-other-files'], ['card.json', undefined], ['preset.risup', undefined],
            ['broken.png', 'read-failed'],
        ])
        expect(mocks.classifyRequests).toEqual([
            [{ source: { type: 'androidSpool', token: 'backup-token' }, displayName: 'backup.risunest' }],
            [{ source: { type: 'androidSpool', token: 'card-token' }, displayName: 'card.json' }],
            [{ source: { type: 'desktopPath', path: presetPath }, displayName: 'preset.risup' }],
        ])
        // The batch outcome reports the import, so the card importer stays quiet, and it ends with the batch.
        expect(mocks.characterProcess).toHaveBeenCalledExactlyOnceWith({ name: 'card.json', data: encode('{"name":"Synthetic"}'), quiet: true, signal: expect.any(AbortSignal) })
        expect(mocks.preset).toHaveBeenCalledExactlyOnceWith({ name: 'preset.risup', data: encode('synthetic preset') })
        expect(outcomes).toEqual([expect.objectContaining({
            state: 'succeeded',
            batch: expect.objectContaining({ entries: [
                { name: 'card.json', state: 'imported', location: { kind: 'character', chaId: 'card.json-id' } },
                { name: 'preset.risup', state: 'imported', location: { kind: 'preset' } },
            ] }),
        })])
        expect(mocks.restoreAndroid).not.toHaveBeenCalled()
        expect(mocks.discarded.sort()).toEqual(['backup-token', 'card-token'])
        expect(mocks.alertError).toHaveBeenCalledExactlyOnceWith(`backup.risunest: ${failureReason('discard-failed')}`)
        // The cache copy belongs to the opened-file queue, which acknowledges it after the delivery.
        expect(mocks.removed).toEqual([])
    })

    it.each([
        ['card.jpg', 'png'],
        ['card.risunest', 'charx'],
        ['card.png', 'jpeg-charx'],
    ] as const)('hands a shared %s to the native importer under the name its spool records', async (name, format) => {
        classify(name, { kind: 'character', format })
        mocks.nativeCharacter.mockResolvedValueOnce({ kind: 'imported', value: 'shared-card' })
        await submitImportDelivery({ origin: 'opened', sources: [spool('card-token', name, 'synthetic card')] })
        expect(mocks.nativeCharacter).toHaveBeenCalledExactlyOnceWith(
            { source: { type: 'androidSpool', token: 'card-token' }, displayName: name },
            expect.anything(),
        )
        expect(outcomes).toEqual([expect.objectContaining({ state: 'succeeded', location: { kind: 'character', chaId: 'shared-card' } })])
    })

    it('releases a lone shared backup when its restore is refused before it starts', async () => {
        classify('backup.risunest', { kind: 'backup', format: 'portable' })
        mocks.restoreAndroid.mockRejectedValueOnce(new NativeFileJobError('generation-active', 'generation-active'))
        await submitImportDelivery({ origin: 'opened', sources: [spool('backup-token', 'backup.risunest', 'synthetic backup')] })
        expect(mocks.restoreAndroid).toHaveBeenCalledExactlyOnceWith({
            source: { type: 'androidSpool', token: 'backup-token' }, displayName: 'backup.risunest', bytes: 16,
        })
        expect(mocks.discarded).toEqual(['backup-token'])
    })

    it('names an opened cache copy after the shared file', async () => {
        classify('preset.risup', { kind: 'preset', format: 'risup' })
        const path = `/data/cache/opened_files/${'a'.repeat(64)}-0-preset.risup`
        mocks.files.set(path, encode('synthetic preset'))
        await submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path }] })
        expect(mocks.classifyRequests).toEqual([[{ source: { type: 'desktopPath', path }, displayName: 'preset.risup' }]])
        expect(mocks.preset).toHaveBeenCalledExactlyOnceWith({ name: 'preset.risup', data: encode('synthetic preset') })
        expect(mocks.removed).toEqual([])
    })

    it('names a restored cache copy after the shared file', async () => {
        classify('backup.risunest', { kind: 'backup', format: 'portable', bytes: 16 })
        const path = `/data/cache/opened_files/${'a'.repeat(64)}-0-backup.risunest`
        await submitImportDelivery({ origin: 'opened', sources: [{ type: 'path', path }] })
        expect(mocks.restore).toHaveBeenCalledExactlyOnceWith(
            { type: 'desktopPath', path },
            { sourceInfo: { name: 'backup.risunest', bytes: 16 } },
        )
    })
})

function dragEvent(type: string, files: File[], types = ['Files']) {
    const event = new Event(type, { bubbles: true, cancelable: true })
    Object.defineProperty(event, 'dataTransfer', { value: { types, files, dropEffect: 'none' } })
    return event as DragEvent
}

describe('drops on the window', () => {
    let dispose: () => void

    beforeEach(() => {
        classify('card.png', { kind: 'character', format: 'png' })
        dispose = installImportIntake()
    })
    afterEach(() => {
        dispose()
        onboardingHold.set(false)
        setAppLockInputBlocked(false, false)
        loadedStore.set(true)
        workingSetBlocked.set(false)
        showUpdatePopup(false)
        closeTextEditorPopup(editorRequest)
    })

    it('shows the overlay during a file drag and imports the dropped files', async () => {
        document.body.dispatchEvent(dragEvent('dragenter', []))
        expect(get(importDropOverlayVisible)).toBe(true)
        const event = dragEvent('drop', [dropped('card.png')])
        document.body.dispatchEvent(event)
        expect(event.defaultPrevented).toBe(true)
        expect(get(importDropOverlayVisible)).toBe(false)
        await vi.waitFor(() => expect(outcomes).toHaveLength(1))
        expect(mocks.characterPath).toHaveBeenCalledOnce()
    })

    it('hides the overlay when the drag leaves after the element it entered first was removed', () => {
        const first = document.createElement('div')
        const second = document.createElement('div')
        document.body.append(first, second)
        first.dispatchEvent(dragEvent('dragenter', []))
        first.remove()
        second.dispatchEvent(dragEvent('dragenter', []))
        // The removed element's dragleave is dispatched while it is detached, so it never reaches the document.
        first.dispatchEvent(dragEvent('dragleave', []))
        expect(get(importDropOverlayVisible)).toBe(true)
        second.dispatchEvent(dragEvent('dragleave', []))
        expect(get(importDropOverlayVisible)).toBe(false)
        second.remove()
    })

    it.each([
        ['onboarding', () => { mocks.db.didFirstSetup = false }],
        ['the onboarding hold', () => onboardingHold.set(true)],
        ['the app lock', () => setAppLockInputBlocked(true, false)],
        ['loading', () => loadedStore.set(false)],
    ])('does nothing during %s but still keeps the WebView from opening the file', async (_name, refuse) => {
        refuse()
        const over = dragEvent('dragover', [])
        document.body.dispatchEvent(dragEvent('dragenter', []))
        document.body.dispatchEvent(over)
        expect(get(importDropOverlayVisible)).toBe(false)
        expect(over.dataTransfer!.dropEffect).toBe('none')
        const event = dragEvent('drop', [dropped('card.png')])
        document.body.dispatchEvent(event)
        expect(event.defaultPrevented).toBe(true)
        await new Promise(resolve => setTimeout(resolve, 0))
        expect(mocks.files.size).toBe(0)
        expect(outcomes).toEqual([])
    })

    it.each([
        ['the library recovery screen', () => workingSetBlocked.set(true), () => workingSetBlocked.set(false)],
        ['the update popup', () => showUpdatePopup(true), () => showUpdatePopup(false)],
        ['a full-screen editor', () => openTextEditorPopup(editorRequest), () => closeTextEditorPopup(editorRequest)],
    ])('takes a drop over %s and imports it once that screen closes', async (_name, open, close) => {
        open()
        const over = dragEvent('dragover', [])
        document.body.dispatchEvent(dragEvent('dragenter', []))
        document.body.dispatchEvent(over)
        expect(get(importDropOverlayVisible)).toBe(true)
        expect(over.dataTransfer!.dropEffect).toBe('copy')
        document.body.dispatchEvent(dragEvent('drop', [dropped('card.png')]))
        await new Promise(resolve => setTimeout(resolve, 0))
        expect(mocks.files.size).toBe(0)
        expect(mocks.characterPath).not.toHaveBeenCalled()
        close()
        await vi.waitFor(() => expect(outcomes).toHaveLength(1))
        expect(mocks.characterPath).toHaveBeenCalledOnce()
    })

    it('leaves in-app drags alone', () => {
        const event = dragEvent('drop', [], ['Files', RISU_APP_INTERNAL_DRAG_TYPE])
        document.body.dispatchEvent(dragEvent('dragenter', [], ['Files', RISU_APP_INTERNAL_DRAG_TYPE]))
        document.body.dispatchEvent(event)
        expect(get(importDropOverlayVisible)).toBe(false)
        expect(event.defaultPrevented).toBe(false)
    })
})

describe('the web build', () => {
    it('installs no window listeners and imports a dropped file through the legacy handler', async () => {
        mocks.runtime = 'web'
        const dispose = installImportIntake()
        document.body.dispatchEvent(dragEvent('dragenter', []))
        expect(get(importDropOverlayVisible)).toBe(false)

        const file = dropped('card.png')
        const event = dragEvent('drop', [file])
        await handleAppDrop(event)
        dispose()
        expect(event.defaultPrevented).toBe(true)
        expect(mocks.characterProcess).toHaveBeenCalledExactlyOnceWith({ name: 'card.png', data: file })
        expect(mocks.classifyRequests).toEqual([])
        expect(outcomes).toEqual([])
    })
})
