import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { language } from 'src/lang'
import type { Database, botPreset } from 'src/ts/storage/database.svelte'
import { DBState } from 'src/ts/stores.svelte'
import { observeEffectivePresetDerive, presetMirrorMap, setEffectivePresetBinding } from 'src/ts/storage/effectiveIdentityState'
import { flushPresetEditorEdits, syncPresetEditorTarget } from 'src/ts/storage/presetEditor.svelte'
import { encodeWithTokenizer } from 'src/ts/tokenizer'
import TextEditorPreview from './TextEditorPreview.svelte'

// The settings window shows.
vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} as Database })
    return {
        DBState, selIdState: { selId: 0 }, selectedCharID: writable(0), settingsOpen: writable(true),
        MobileGUI: writable(false), MobileGUIStack: writable(0), MobileSideBar: writable(0),
    } satisfies Partial<typeof import('src/ts/stores.svelte')>
})
vi.mock('src/ts/storage/database.svelte', async () => {
    const { DBState } = await import('src/ts/stores.svelte')
    return {
        appVer: '1.0.0', getDatabase: () => DBState.db, getCurrentCharacter: () => DBState.db.characters[0],
        getCurrentChat: () => DBState.db.characters[0]?.chats[0],
    } satisfies Partial<typeof import('src/ts/storage/database.svelte')>
})
vi.mock('src/ts/globalApi.svelte', () => ({ aiWatermarkingLawApplies: () => false, getFileSrc: async () => '', globalFetch: vi.fn() }))
vi.mock('src/ts/plugins/plugins.svelte', () => ({ pluginV2: { providerOptions: new Map() } }))
const toggles = vi.hoisted(() => ({ props: undefined as { presetDatabase?: Database, editorPreview?: boolean } | undefined }))
// The toggle list is covered by Toggles.svelte.test.ts; here it exposes the preset it receives.
vi.mock('../SideBars/Toggles.svelte', () => ({ default: (_anchor: unknown, props: typeof toggles.props) => { toggles.props = props } }))

function apply(db: Database, preset: botPreset): void {
    const fields = db as unknown as Record<string, unknown>
    const record = preset as unknown as Record<string, unknown>
    for (const [key, field] of Object.entries(presetMirrorMap)) {
        if (Object.hasOwn(record, field)) fields[key] = $state.snapshot(record[field])
    }
}

let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    observeEffectivePresetDerive(undefined)
    document.body.replaceChildren()
})

const text = '합성 테스트 문장입니다. Synthetic sentence.'

async function preview(presetEditor: boolean, tokens: number): Promise<[string, string | undefined]> {
    instance = mount(TextEditorPreview, { target: document.body, props: { value: '{{mainprompt}}{{moduleenabled::synthetic}}', presetEditor } })
    await tick()
    const parsed = document.querySelector('[data-text-editor-preview]')!.textContent!
    await vi.waitFor(() => expect(document.body.textContent).toContain(`${language.tokens}: ${tokens}`))
    toggles.props = undefined
    const showToggles = [...document.querySelectorAll('button')].find(button => button.textContent === language.customPromptTemplateToggle)!
    showToggles.click()
    await tick()
    const toggleTemplate = toggles.props?.presetDatabase?.customPromptTemplateToggle
    // The panel leaves out the binding controls other than the toggle binding's save.
    expect(toggles.props?.editorPreview).toBe(true)
    await unmount(instance)
    instance = undefined
    return [parsed, toggleTemplate]
}

it('previews, counts and lists toggles with the preset the settings window edits when opened from it', async () => {
    DBState.db = {
        botPresetsId: 0,
        botPresets: [
            { id: 'selected', name: 'Selected', mainPrompt: text, aiModel: 'gpt4', customPromptTemplateToggle: 'selected=Selected', moduleIntergration: 'synthetic' },
            { id: 'bound', name: 'Bound', mainPrompt: `Bound ${text}`, aiModel: 'gpt4o', customPromptTemplateToggle: 'bound=Bound' },
        ],
        modules: [{ id: 'synthetic-module', name: 'Synthetic module', namespace: 'synthetic', description: '' }],
        characters: [{ chaId: 'synthetic', name: 'Synthetic', chatPage: 0, chats: [{ message: [], scriptstate: {} }] }],
        globalChatVariables: {},
        customTokenizer: 'tik',
        currentPluginProvider: '',
    } as unknown as Database
    observeEffectivePresetDerive({
        beforeDerive: flushPresetEditorEdits,
        afterDerive: (db) => syncPresetEditorTarget(db, apply),
    })
    setEffectivePresetBinding(DBState.db, 'bound', apply)

    // The presets use different tokenizers, so the count shows which model it follows.
    const selectedTokens = (await encodeWithTokenizer(`${text}1`, 'tik')).length
    const boundTokens = (await encodeWithTokenizer(`Bound ${text}0`, 'o200k_base')).length
    expect(selectedTokens).not.toBe(boundTokens)
    // Only the selected preset enables the module.
    expect(await preview(true, selectedTokens)).toEqual([`${text}1`, 'selected=Selected'])
    expect(await preview(false, boundTokens)).toEqual([`Bound ${text}0`, 'bound=Bound'])
})
