import { imageFixture } from '../../../tests/support/imageFixtures'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { languageEnglish } from 'src/lang/en'
import { DBState, CharConfigSubMenu, selectedCharID } from 'src/ts/stores.svelte'
import CharConfig from './CharConfig.svelte'

const mocks = vi.hoisted(() => ({
    alertError: vi.fn(), selectMultiple: vi.fn(), selectSingle: vi.fn(), importRegex: vi.fn(), registerModel: vi.fn(),
    saveAsset: vi.fn(async () => 'assets/synthetic.bin'), mutate: vi.fn(), fileSrc: vi.fn(async () => ''),
}))
vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} })
    return { DBState, CharConfigSubMenu: writable(4), selectedCharID: writable(0),
        MobileGUI: writable(false), ShowRealmFrameStore: writable(false), hypaV3ModalOpen: writable(false) }
})
vi.mock('src/ts/tokenizer', () => ({ tokenizeAccurate: vi.fn(async () => 1) }))
vi.mock('src/ts/storage/database.svelte', () => ({}))
vi.mock('src/ts/characters', () => ({ getCharImage: vi.fn(async () => ''), addingEmotion: writable(false) }))
vi.mock('src/ts/storage/characterArchive', () => ({ archiveIsAvailable: () => false }))
vi.mock('src/ts/alert', () => ({ alertError: mocks.alertError }))
vi.mock('src/ts/util', async importOriginal => ({ ...await importOriginal<typeof import('src/ts/util')>(), selectMultipleFile: mocks.selectMultiple, selectSingleFile: mocks.selectSingle }))
vi.mock('src/ts/characterCards', () => ({}))
vi.mock('src/ts/process/tts', () => ({}))
vi.mock('src/ts/globalApi.svelte', () => ({ getFileSrc: mocks.fileSrc, saveAsset: mocks.saveAsset }))
vi.mock('src/ts/process/inlayScreen', () => ({}))
vi.mock('src/ts/process/transformers', () => ({ registerOnnxModel: mocks.registerModel }))
vi.mock('src/ts/process/modules', () => ({}))
vi.mock('src/ts/process/scripts', () => ({ importRegex: mocks.importRegex }))
vi.mock('src/ts/interchangeability', () => ({}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({ mutatePersistentCharacterDetail: mocks.mutate }))
vi.mock('./LoreBook/LoreBookSetting.svelte', () => ({ default: () => {} }))
vi.mock('./Scripts/RegexList.svelte', () => ({ default: () => {} }))
vi.mock('./Scripts/TriggerList.svelte', () => ({ default: () => {} }))
vi.mock('./OnboardingEditor.svelte', () => ({ default: () => {} }))
vi.mock('./Toggles.svelte', () => ({ default: () => {} }))
vi.mock('../UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))
vi.mock('../UI/GUI/MultiLangInput.svelte', () => ({ default: () => {} }))
vi.mock('../Others/Help.svelte', () => ({ default: () => {} }))
// The editor itself is covered by ToggleDefinitionEditor.svelte.test.ts; here it exposes what it receives.
const toggleEditor = vi.hoisted(() => ({ props: {} as { value?: unknown, onChange?: (value: string) => void } }))
vi.mock('../UI/ToggleDefinitionEditor.svelte', () => ({
    default: (_anchor: unknown, props: typeof toggleEditor.props) => { toggleEditor.props = props },
}))
vi.mock('src/lib/Gallery/AssetGallery.svelte', () => ({ default: () => {} }))

let instance: ReturnType<typeof mount> | undefined
function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>(done => { resolve = done })
    return { promise, resolve }
}
async function setup(menu: number) {
    CharConfigSubMenu.set(menu)
    instance = mount(CharConfig, { target: document.body })
    await tick()
}
function button(text: string) {
    return [...document.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent?.trim() === text)!
}
beforeEach(() => {
    vi.clearAllMocks()
    mocks.fileSrc.mockReset().mockResolvedValue('')
    selectedCharID.set(0)
    DBState.db = { characters: ['First', 'Second'].map(chaId => ({
        chaId, type: 'character', name: chaId, chatPage: 0, chats: [{ note: '' }],
        customscript: [], additionalAssets: [], emotionImages: [], desc: '', firstMessage: '', ttsMode: '',
    })) } as any
    mocks.mutate.mockImplementation(async (id, _reason, mutate) => {
        const character = DBState.db.characters.find(character => character.chaId === id)
        if (!character) return false
        mutate({ character })
        return true
    })
})
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    document.body.replaceChildren()
})

it('imports regex into the current record of the original character after replacement and navigation', async () => {
    const selection = deferred<any[]>()
    mocks.importRegex.mockReturnValueOnce(selection.promise)
    await setup(4)
    Array.from(document.querySelectorAll<HTMLButtonElement>('button.font-medium')).at(-1)!.click()
    expect(mocks.importRegex).toHaveBeenCalledOnce()
    DBState.db.characters[0] = { ...DBState.db.characters[0], customscript: [
        { comment: 'Concurrent', in: '', out: '', type: 'editinput' },
    ] }
    selectedCharID.set(1)
    await tick()
    selection.resolve([{ comment: 'Imported', in: '', out: '', type: 'editinput' }])
    await vi.waitFor(() => expect(DBState.db.characters[0].customscript.map(script => script.comment)).toEqual(['Concurrent', 'Imported']))
    expect(DBState.db.characters[1].customscript).toEqual([])
})

it.each(['vits', 'gptsovits'])('applies a pending %s selection to the original character', async (mode) => {
    DBState.db.characters[0].ttsMode = mode
    const selected = deferred<any>()
    mocks.registerModel.mockReturnValue(selected.promise)
    mocks.selectSingle.mockReturnValue(selected.promise)
    await setup(5)
    if (mode === 'vits') button(languageEnglish.selectModel).click()
    else document.querySelector<HTMLButtonElement>('button.h-10')!.click()
    selectedCharID.set(1)
    await tick()
    selected.resolve(mode === 'vits' ? { name: 'Synthetic model' } : { name: 'Synthetic.wav', data: new Uint8Array([1]) })
    await vi.waitFor(() => expect(mocks.mutate).toHaveBeenCalledOnce())
    expect(mocks.mutate.mock.calls[0][0]).toBe('First')
    expect((DBState.db.characters[1] as any)[mode === 'vits' ? 'vits' : 'gptSoVitsConfig']).toBeUndefined()
})

it('adds files to the original character after selection changes during asset storage', async () => {
    const saved = deferred<string>()
    mocks.selectMultiple.mockResolvedValueOnce([
        { name: 'synthetic.png', data: imageFixture('png') },
        { name: 'bad.PNG', data: new Uint8Array([1]) },
        { name: 'generic.svg', data: new TextEncoder().encode('<svg/>') },
        { name: 'renamed.PNG', data: imageFixture('webp') },
    ])
    mocks.saveAsset.mockReturnValueOnce(saved.promise)
    await setup(1)
    button(languageEnglish.additionalAssets).click()
    await tick()
    document.querySelector<HTMLButtonElement>('table th button')!.click()
    await vi.waitFor(() => expect(mocks.saveAsset).toHaveBeenCalled())
    selectedCharID.set(1)
    await tick()
    saved.resolve('assets/synthetic.png')
    await vi.waitFor(() => expect(DBState.db.characters[0].additionalAssets.map(asset => [asset[0], asset[2]])).toEqual([['synthetic.png', 'png'], ['generic.svg', 'svg'], ['renamed.PNG', 'webp']]))
    expect(DBState.db.characters[1].additionalAssets).toEqual([])
    expect(mocks.alertError).toHaveBeenCalledWith(`bad.PNG: ${languageEnglish.risuNest.imageFormats.unsupported}`)
})

it('ignores an old asset preview that resolves after character navigation', async () => {
    const oldPreview = deferred<string>()
    const currentPreview = 'data:image/png;base64,Ag=='
    mocks.fileSrc.mockReturnValueOnce(oldPreview.promise).mockResolvedValue(currentPreview)
    DBState.db.useAdditionalAssetsPreview = true
    for (const character of DBState.db.characters) {
        character.additionalAssets = [['Synthetic image', `assets/${character.chaId}.png`, 'png']]
    }
    await setup(1)
    button(languageEnglish.additionalAssets).click()
    await tick()
    selectedCharID.set(1)
    await vi.waitFor(() => expect(document.querySelector('table img')?.getAttribute('src')).toBe(currentPreview))
    oldPreview.resolve('data:image/png;base64,AQ==')
    await tick()
    expect(document.querySelector('table img')?.getAttribute('src')).toBe(currentPreview)
})

it('resolves a large additional-asset list only while its submenu is open and ignores late results', async () => {
    const pending = deferred<string>()
    mocks.fileSrc.mockReturnValue(pending.promise)
    DBState.db.useAdditionalAssetsPreview = true
    DBState.db.characters[0].additionalAssets = Array.from({ length: 300 }, (_, index) => [`synthetic-${index}`, `assets/synthetic-${index}.png`, 'png']) as any
    await setup(4)
    expect(mocks.fileSrc).not.toHaveBeenCalled()
    CharConfigSubMenu.set(1)
    await tick()
    expect(mocks.fileSrc).toHaveBeenCalledTimes(300)
    CharConfigSubMenu.set(4)
    await tick()
    pending.resolve('data:,synthetic-late')
    await tick()
    expect(mocks.fileSrc).toHaveBeenCalledTimes(300)
    expect(document.querySelector('img[src="data:,synthetic-late"]')).toBeNull()
})

it('offers the asset style with new image handling even when the asset prompt is off', async () => {
    const assetStyle = () => [...document.querySelectorAll('span')].find(span => span.textContent?.trim() === languageEnglish.assetStyle)
    DBState.db.newImageHandlingBeta = true
    DBState.db.characters[0].prebuiltAssetCommand = false
    await setup(1)
    button(languageEnglish.additionalAssets).click()
    await tick()
    const select = assetStyle()!.nextElementSibling as HTMLSelectElement
    expect([...select.options].map(option => option.value)).toEqual(['', 'dynamic'])
    DBState.db.newImageHandlingBeta = false
    await tick()
    expect(assetStyle()).toBeUndefined()
})

it('edits the selected character custom toggles through the toggle editor', async () => {
    DBState.db.characters.forEach(character => Object.assign(character, {
        customModuleToggle: `${character.chaId}=On`, bias: [], alternateGreetings: [], personality: '', scenario: '',
        additionalData: {}, depth_prompt: { depth: 0, prompt: '' },
    }))
    selectedCharID.set(1)
    await setup(2)
    expect(toggleEditor.props.value).toBe('Second=On')
    toggleEditor.props.onChange!('edited=Edited')
    expect(DBState.db.characters.map(character => character.customModuleToggle)).toEqual(['First=On', 'edited=Edited'])
})
