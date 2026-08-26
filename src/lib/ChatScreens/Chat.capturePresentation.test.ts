// @vitest-environment happy-dom

import { writable } from 'svelte/store'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'

const live = vi.hoisted(() => ({
    db: {} as Record<string, any>,
}))

vi.mock('./ChatBody.svelte', async () => ({
    default: (await import('./ChatBodyCaptureProbe.test.svelte')).default,
}))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: { get db() { return live.db } },
    ReloadChatPointer: writable([]),
    CurrentTriggerIdStore: writable(null),
    popupStore: writable(null),
    selectedCharID: writable(0),
    HideIconStore: writable(false),
    ReloadGUIPointer: writable(0),
    selIdState: { selId: 0 },
}))
vi.mock('src/ts/gui/colorscheme', () => ({ ColorSchemeTypeStore: writable('light') }))
vi.mock('src/ts/globalApi.svelte', () => ({
    aiLawApplies: () => false,
    changeChatTo: vi.fn(),
    foldChatToMessage: vi.fn(),
    getFileSrc: vi.fn(async (source: string) => source),
    createChatCopyName: vi.fn(),
}))
vi.mock('src/ts/process/scripts', () => ({
    risuChatParser: (value: string) => value,
}))
vi.mock('src/ts/model/modellist', () => ({
    getModelInfo: (model: string) => ({ shortName: model || 'model' }),
}))
vi.mock('src/ts/process/scriptings', () => ({ runLuaButtonTrigger: vi.fn() }))
vi.mock('src/ts/process/triggers', () => ({ runTrigger: vi.fn() }))
vi.mock('src/ts/process/tts', () => ({ sayTTS: vi.fn() }))
vi.mock('src/ts/sync/multiuser', () => ({ ConnectionOpenStore: writable(false) }))
vi.mock('src/ts/util', () => ({
    capitalize: (value: string) => value,
    getUserIcon: () => '',
    getUserName: () => 'Live User',
    sleep: () => Promise.resolve(),
}))
vi.mock('../../lang', () => ({
    language: {
        branchedText: 'Branched from {}',
        noMessage: 'No message',
    },
}))
vi.mock('../../ts/alert', () => ({
    alertClear: vi.fn(), alertConfirm: vi.fn(), alertInput: vi.fn(), alertNormal: vi.fn(),
    alertRequestData: vi.fn(), alertWait: vi.fn(),
}))
vi.mock('../../ts/translator/translator', () => ({ getLLMCache: vi.fn(), setLLMCache: vi.fn() }))
vi.mock('src/ts/process/files/inlayRenderSource', () => ({
    DeferredInlayMarkerRegistry: class {}, withResolvedDeferredInlaySources: vi.fn(),
}))
vi.mock('src/ts/process/files/chatCopyInlays', () => ({ copyImageSourceToDataUrl: vi.fn() }))

import Chat from './Chat.svelte'

function context(overrides: Record<string, unknown> = {}) {
    const character = {
        type: 'character' as const,
        name: 'Frozen Character',
        chaId: 'frozen',
        chatPage: 0,
        chats: [{ message: [], note: '', name: '', localLore: [], bookmarks: [] }],
        customscript: [],
    }
    return {
        character: null,
        characterName: 'Frozen Character',
        characterImageSource: '',
        characterLargePortrait: false,
        userName: 'Frozen User',
        userImageSource: '',
        userLargePortrait: false,
        moduleAssets: [],
        presetRegex: [],
        moduleRegexScripts: [],
        assetStyle: '',
        parserContext: {
            database: { characters: [character] } as any,
            character: character as any,
            userName: 'Frozen User',
            personaPrompt: 'Frozen Persona',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
        },
        totalTurns: 4,
        selectionStart: 1,
        firstParserMessageIndex: 0,
        settings: {
            autoTranslate: false,
            autoTranslateCachedOnly: false,
            translatorType: 'google',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            newImageHandlingBeta: false,
            assetWidth: -1,
            hideAllImages: false,
            iconSize: 100,
            zoomSize: 100,
            lineHeight: 1.25,
            dynamicAssets: false,
            dynamicAssetsEditDisplay: false,
            legacyMediaFindings: false,
            assetMaxDifference: 0.5,
            theme: '',
            guiHTML: '',
            roundIcons: false,
            hideIcons: false,
            proseInvert: false,
            requestInfoInsideChat: false,
            aiLawApplies: false,
            translator: '',
            swipe: true,
            showFirstMessagePages: true,
            memoryLimitThickness: 1,
            customQuotes: false,
            customQuotesData: ['“', '”', '‘', '’'] as [string, string, string, string],
            unformatQuotes: false,
            blockquoteStyling: false,
            ...overrides,
        },
    }
}

describe('Chat frozen capture presentation', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        live.db = {
            theme: 'cardboard',
            iconsize: 25,
            zoomsize: 25,
            lineHeight: 3,
            roundIcons: true,
            memoryLimitThickness: 9,
            characters: [],
        }
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        document.body.replaceChildren()
    })

    test('renders normal branch comment presentation from the frozen message', async () => {
        const message = {
            role: 'char' as const,
            data: '{{specialcomment::branchedfrom::chat-id::Frozen Branch::message-id::}}',
            isComment: true,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen Character',
                role: 'char',
                idx: 1,
                totalLength: 4,
                isLastMemory: false,
                isComment: true,
                captureMessage: message,
                captureContext: context(),
            },
        })

        await vi.waitFor(() => expect(target.querySelector('button')?.textContent).toContain('Frozen Branch'))
        expect(target.querySelector('[data-chat-body-probe]')).toBeNull()
    })

    test('keeps the frozen mobile theme and timestamp after live presentation state changes', async () => {
        const timestamp = Date.UTC(2024, 0, 2, 3, 4, 5)
        const frozen = context({ theme: 'mobilechat' })
        live.db.theme = 'customHTML'
        live.db.guiHTML = '<div>Live custom</div>'
        const message = { role: 'user' as const, data: 'Frozen body', time: timestamp }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen User',
                role: 'user',
                idx: 2,
                totalLength: 4,
                isLastMemory: false,
                captureMessage: message,
                captureContext: frozen,
            },
        })

        await vi.waitFor(() => expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe('Frozen body'))
        expect(target.querySelector('.bg-gray-100')).not.toBeNull()
        expect(target.textContent).not.toContain('Live custom')
        expect(target.querySelector('.text-xs')?.textContent?.trim()).not.toBe('')
    })

    test('renders frozen custom HTML with the normal text box slot', async () => {
        const frozen = context({
            theme: 'customHTML',
            guiHTML: '<div class="capture-custom"><span>Frozen layout</span><RISUTEXTBOX></RISUTEXTBOX></div>',
        })
        live.db.theme = 'mobilechat'
        const message = { role: 'char' as const, data: 'Frozen custom body' }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen Character',
                role: 'char',
                idx: 2,
                totalLength: 4,
                isLastMemory: false,
                captureMessage: message,
                captureContext: frozen,
            },
        })

        await vi.waitFor(() => expect(target.querySelector('.capture-custom')).not.toBeNull())
        expect(target.textContent).toContain('Frozen layout')
        expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe('Frozen custom body')
    })

    test('keeps generation information and all-before boundary presentation', async () => {
        const frozen = context({ requestInfoInsideChat: true })
        const message = {
            role: 'char' as const,
            data: 'Generated',
            disabled: 'allBefore' as const,
            generationInfo: { model: 'Frozen Model' },
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen Character',
                role: 'char',
                idx: 3,
                totalLength: 4,
                isLastMemory: false,
                disabled: 'allBefore',
                messageGenerationInfo: message.generationInfo,
                captureMessage: message,
                captureContext: frozen,
            },
        })

        await vi.waitFor(() => expect(target.textContent).toContain('Frozen Model'))
        expect(target.querySelector('.border-amber-500')).not.toBeNull()
    })
})
