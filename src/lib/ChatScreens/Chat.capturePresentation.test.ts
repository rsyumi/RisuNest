// @vitest-environment happy-dom

import { writable } from 'svelte/store'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import { ActiveConversationSession } from 'src/ts/storage/activeConversationSession'
import type { character, Chat as ChatRecord, Message } from 'src/ts/storage/database.svelte'
import type { ConversationViewportKey } from 'src/ts/conversationViewportSource'
import type { SelectedConversationOperations } from 'src/ts/selectedConversationOperations'

const live = vi.hoisted(() => ({
    db: {} as Record<string, any>,
}))
const runtime = vi.hoisted(() => ({
    activeSession: null as unknown,
    persistent: {} as Record<string, unknown>,
}))
const actionMocks = vi.hoisted(() => ({
    runTrigger: vi.fn(),
    runLuaButtonTrigger: vi.fn(),
}))
const parserCalls = vi.hoisted(() => [] as Array<{
    chara?: unknown
    role?: string
    chatID?: number
    projectedChatID?: number
    historyOffset?: number
}>)

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
    risuChatParser: (value: string, arg: {
        chara?: any
        role?: string
        chatID?: number
        projectedChatID?: number
    } = {}) => {
        parserCalls.push({
            chara: arg.chara,
            role: arg.role,
            chatID: arg.chatID,
            projectedChatID: arg.projectedChatID,
            historyOffset: (arg as any).historyOffset,
        })
        if (!value.includes('{{char}}')) return value
        if (typeof arg.chara === 'string') return value.replaceAll('{{char}}', arg.chara)
        if (arg.chara?.type === 'group') {
            const message = arg.chara.chats[arg.chara.chatPage].message.at(-1)
            const member = arg.chara.characters
                .map((id: string) => live.db.characters.find((candidate: any) => candidate.chaId === id))
                .find((candidate: any) => candidate?.chaId === message?.saying)
            return value.replaceAll('{{char}}', member?.name ?? arg.chara.name)
        }
        return value.replaceAll('{{char}}', arg.chara?.name ?? '')
    },
}))
vi.mock('src/ts/model/modellist', () => ({
    getModelInfo: (model: string) => ({ shortName: model || 'model' }),
}))
vi.mock('src/ts/process/scriptings', () => ({
    runLuaButtonTrigger: actionMocks.runLuaButtonTrigger,
}))
vi.mock('src/ts/process/triggers', () => ({ runTrigger: actionMocks.runTrigger }))
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
vi.mock('../../ts/storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => runtime.activeSession,
    getPersistentDataRuntime: () => runtime.persistent,
}))

import type { ProcessScriptCaptureContext } from 'src/ts/process/scripts'
import Chat from './Chat.svelte'
import ChatCaptureBatchHarness from './ChatCaptureBatchHarness.test.svelte'

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

function makeWindowedEditHarness() {
    const message: Message = {
        role: 'char',
        data: 'Original viewport message',
        chatId: 'message-1',
    }
    const completeConversation = {
        id: 'conversation-a',
        name: 'Conversation',
        note: '',
        localLore: [],
        message: [{ role: 'user' as const, data: 'zero' }, message],
        bookmarks: [],
    } as ChatRecord
    const completeCharacter = {
        type: 'character',
        name: 'Live Character',
        chaId: 'character-a',
        chatPage: 0,
        chats: [completeConversation],
        ttsMode: 'none',
    } as unknown as character
    const metadataConversation = {
        id: completeConversation.id,
        bookmarks: [],
    } as unknown as character['chats'][number]
    Object.defineProperty(metadataConversation, 'message', {
        get() {
            throw new Error('metadata-only conversation body was accessed')
        },
    })
    const metadataCharacter = {
        ...completeCharacter,
        chats: [metadataConversation],
    }
    const session = new ActiveConversationSession({
        characterId: completeCharacter.chaId,
        conversationId: completeConversation.id,
        conversation: completeConversation,
        storeRevision: 7,
    })
    const release = vi.fn()
    const intent = {
        selection: {
            characterId: completeCharacter.chaId,
            conversationId: completeConversation.id,
            navigationGeneration: 1,
            storeRevision: 7,
        },
        absoluteIndex: 1,
        sourceToken: 'source-a',
        sourceVersion: 3,
        rowKey: 'row-1' as ConversationViewportKey,
        messageEvidence: message,
    }
    const captureMessageEditIntent = vi.fn(() => intent)
    const acquireTarget = vi.fn(async () => {
        live.db.characters = [completeCharacter]
        runtime.activeSession = session
        const locator = session.locate(1)
        return {
            target: {
                kind: 'session' as const,
                absoluteIndex: 1,
                character: completeCharacter,
                conversation: completeConversation,
                message: session.readMessage(locator),
                session,
                locator,
            },
            release,
        }
    })
    const acquireCompleteMessageTargetForIntent = acquireTarget
    const acquireCompleteMessageTarget = acquireTarget
    const withCompleteSelectedConversation = vi.fn()
    const operations = {
        captureMessageEditIntent,
        acquireCompleteMessageTargetForIntent,
        acquireCompleteMessageTarget,
        withCompleteSelectedConversation,
    } as unknown as SelectedConversationOperations
    return {
        message,
        metadataCharacter,
        completeConversation,
        operations,
        captureMessageEditIntent,
        acquireCompleteMessageTargetForIntent,
        acquireCompleteMessageTarget,
        completeCharacter,
        session,
        withCompleteSelectedConversation,
        release,
    }
}

describe('Chat frozen capture presentation', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        parserCalls.length = 0
        actionMocks.runTrigger.mockReset()
        actionMocks.runLuaButtonTrigger.mockReset()
        live.db = {
            theme: 'cardboard',
            iconsize: 25,
            zoomsize: 25,
            lineHeight: 3,
            roundIcons: true,
            memoryLimitThickness: 9,
            characters: [],
        }
        runtime.activeSession = null
        runtime.persistent = {}
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

    test('uses the mounted viewport row for live presentation without recapturing its array index', async () => {
        const timestamp = Date.UTC(2024, 5, 6, 7, 8, 9)
        const message = {
            role: 'char' as const,
            data: 'Viewport body',
            chatId: 'viewport-message',
            time: timestamp,
        }
        const indexReads = vi.fn(() => {
            throw new Error('live message index was recaptured')
        })
        const messages = new Proxy([] as typeof message[], {
            get(target, property, receiver) {
                if (property === '3') return indexReads()
                return Reflect.get(target, property, receiver)
            },
        })
        live.db = {
            ...live.db,
            theme: 'mobilechat',
            characters: [{
                type: 'character',
                name: 'Live Character',
                chaId: 'live-character',
                chatPage: 0,
                chats: [{ id: 'live-chat', message: messages, bookmarks: [] }],
                ttsMode: 'none',
            }],
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Live Character',
                role: 'char',
                idx: 3,
                totalLength: 10,
                isLastMemory: false,
                viewportRow: {
                    key: 'viewport-key' as any,
                    absoluteIndex: 3,
                    message,
                    sourceVersion: 1,
                },
                captureViewportTarget: () => null,
                bookmarked: false,
            },
        })

        await vi.waitFor(() => expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe('Viewport body'))
        expect(target.querySelector('[data-chat-id="viewport-message"]')).not.toBeNull()
        expect(target.querySelector('.text-xs')?.textContent?.trim()).not.toBe('')
        expect(indexReads).not.toHaveBeenCalled()
    })

    test('does not recapture optional presentation fields missing from a viewport row', async () => {
        const message = {
            role: 'char' as const,
            data: 'Viewport body without optional fields',
        }
        const indexReads = vi.fn(() => {
            throw new Error('live message index was recaptured')
        })
        const messages = new Proxy([] as typeof message[], {
            get(target, property, receiver) {
                if (property === '3') return indexReads()
                return Reflect.get(target, property, receiver)
            },
        })
        live.db = {
            ...live.db,
            theme: 'mobilechat',
            characters: [{
                type: 'character',
                name: 'Live Character',
                chaId: 'live-character',
                chatPage: 0,
                chats: [{ id: 'live-chat', message: messages, bookmarks: [] }],
                ttsMode: 'none',
            }],
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Live Character',
                role: 'char',
                idx: 3,
                totalLength: 10,
                isLastMemory: false,
                viewportRow: {
                    key: 'viewport-key' as any,
                    absoluteIndex: 3,
                    message,
                    sourceVersion: 1,
                },
                captureViewportTarget: () => null,
            },
        })

        await vi.waitFor(() => expect(
            target.querySelector('[data-chat-body-probe]')?.textContent,
        ).toBe(message.data))
        expect(target.querySelector('[data-chat-id=""]')).not.toBeNull()
        expect(indexReads).not.toHaveBeenCalled()
    })

    test('uses a bounded live parser projection without capture-only UI semantics', async () => {
        const projected = context()
        const parserContext = projected.parserContext as ProcessScriptCaptureContext['parserContext']
        parserContext.historyOffset = 4
        parserContext.character.chats[0].message = [
            { role: 'char', data: 'previous' },
            { role: 'user', data: 'nearby' },
            { role: 'char', data: '{{char}}' },
        ] as any
        parserContext.database.characters[0] = parserContext.character
        const parserProjection = {
            kind: 'bounded' as const,
            characterId: 'frozen',
            conversationId: parserContext.character.chats[0].id ?? 'live-chat',
            revision: 1,
            totalMessages: 7,
            chatID: 6,
            projectedChatID: 2,
            historyOffset: 4,
            messages: parserContext.character.chats[0].message,
            context: {
                presetRegex: projected.presetRegex,
                moduleRegexScripts: projected.moduleRegexScripts,
                moduleAssets: projected.moduleAssets,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                parserContext,
            },
        }
        live.db = {
            ...live.db,
            theme: '',
            clickToEdit: false,
            characters: [{
                type: 'character',
                name: 'Live Character',
                chaId: 'live-character',
                chatPage: 0,
                chats: [{ id: 'live-chat', message: [] }],
                ttsMode: 'none',
            }],
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: '{{char}}',
                name: 'Live Character',
                role: 'char',
                idx: 6,
                totalLength: 7,
                isLastMemory: false,
                parserProjection: parserProjection as any,
            },
        })

        await vi.waitFor(() => expect(
            target.querySelector('[data-chat-body-probe]')?.textContent,
        ).toBe('Frozen Character'))
        expect(parserCalls).toContainEqual(expect.objectContaining({
            chatID: 6,
            projectedChatID: 2,
            historyOffset: 4,
        }))
        expect(target.querySelector('[data-chat-body-probe]')?.getAttribute(
            'data-parser-projection',
        )).toBe('bounded')
        expect(target.querySelector('.button-icon-edit')).not.toBeNull()
    })

    test('delays windowed edit promotion until save and commits against the original row target', async () => {
        const harness = makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            swipe: false,
            clickToEdit: false,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })

        const editButton = await vi.waitFor(() => {
            const button = target.querySelector<HTMLButtonElement>('.button-icon-edit')
            expect(button).not.toBeNull()
            return button!
        })
        editButton.click()
        expect(harness.captureMessageEditIntent).toHaveBeenCalledOnce()
        expect(harness.acquireCompleteMessageTargetForIntent).not.toHaveBeenCalled()

        const editor = await vi.waitFor(() => {
            const textarea = target.querySelector<HTMLTextAreaElement>('.message-edit-area')
            expect(textarea).not.toBeNull()
            return textarea!
        })
        editor.value = 'Saved after promotion'
        editor.dispatchEvent(new Event('input', { bubbles: true }))
        editButton.click()

        await vi.waitFor(() => {
            expect(harness.completeConversation.message[1].data).toBe('Saved after promotion')
            expect(harness.acquireCompleteMessageTargetForIntent).toHaveBeenCalledWith(
                expect.objectContaining({ rowKey: 'row-1' }),
                'edit-message',
            )
            expect(harness.release).toHaveBeenCalledOnce()
        })
    })

    test('promotes and releases a windowed message operation before removing its row', async () => {
        const harness = makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            swipe: false,
            askRemoval: false,
            instantRemove: false,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })

        const removeButton = await vi.waitFor(() => {
            const button = target.querySelector<HTMLButtonElement>('.button-icon-remove')
            expect(button).not.toBeNull()
            return button!
        })
        removeButton.dispatchEvent(new MouseEvent('click', { bubbles: true }))

        await vi.waitFor(() => {
            expect(harness.acquireCompleteMessageTarget).toHaveBeenCalledWith(1, 'remove-message')
            expect(harness.session.totalMessages).toBe(1)
            expect(harness.release).toHaveBeenCalledOnce()
        })
    })

    test('runs a manual windowed trigger only inside the complete conversation gateway', async () => {
        const harness = makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            swipe: false,
        }
        const requireCurrent = vi.fn(() => ({
            character: harness.completeCharacter,
            conversation: harness.completeConversation,
            session: harness.session,
            selection: {
                characterId: harness.completeCharacter.chaId,
                conversationId: harness.completeConversation.id!,
                navigationGeneration: 1,
                storeRevision: 7,
            },
        }))
        harness.withCompleteSelectedConversation.mockImplementation(
            async (_reason, operation) => operation({ requireCurrent }),
        )
        actionMocks.runTrigger.mockResolvedValue(null)
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })

        const body = await vi.waitFor(() => {
            const element = target.querySelector<HTMLElement>('[data-chat-body-probe]')
            expect(element).not.toBeNull()
            return element!
        })
        const trigger = document.createElement('button')
        trigger.setAttribute('risu-trigger', 'manual-test')
        body.append(trigger)
        trigger.click()

        await vi.waitFor(() => {
            expect(harness.withCompleteSelectedConversation).toHaveBeenCalledWith(
                'manual-chat-trigger',
                expect.any(Function),
            )
            expect(actionMocks.runTrigger).toHaveBeenCalledWith(
                harness.completeCharacter,
                'manual',
                expect.objectContaining({ chat: harness.completeConversation }),
            )
            expect(requireCurrent).toHaveBeenCalledTimes(2)
        })
    })

    test('renders each frozen group turn with the same names as the normal Chat presentation', async () => {
        const memberA = { ...context().parserContext.character, name: 'Member A', chaId: 'member-a' }
        const memberB = { ...context().parserContext.character, name: 'Member B', chaId: 'member-b' }
        const messages = [
            { role: 'char' as const, data: '{{char}}', saying: 'member-a' },
            { role: 'char' as const, data: '{{char}}', saying: 'member-b' },
        ]
        const group = {
            type: 'group' as const,
            name: 'Frozen Group',
            chaId: 'group',
            chatPage: 0,
            chats: [{ message: messages, note: '', name: '', localLore: [], bookmarks: [] }],
            characters: ['member-a', 'member-b'],
            customscript: [],
        }
        const frozen = context()
        frozen.character = null
        frozen.characterName = group.name
        frozen.parserContext.character = group as any
        frozen.parserContext.database = { characters: [group, memberA, memberB] } as any
        live.db.characters = [group, memberA, memberB]

        mounted = mount(ChatCaptureBatchHarness, {
            target,
            props: { messages, captureContext: frozen as any, firstIndex: 4 },
        })

        await vi.waitFor(() => expect(target.querySelectorAll('[data-chat-body-probe]')).toHaveLength(2))
        expect([...target.querySelectorAll('[data-chat-body-probe]')].map((node) => node.textContent)).toEqual([
            'Frozen Group',
            'Frozen Group',
        ])
        expect(parserCalls.filter((call) => call.role === 'char').map((call) => call.chara)).toEqual([
            'Frozen Group',
            'Frozen Group',
        ])
        expect(parserCalls.filter((call) => call.role === 'char').map((call) => [
            call.chatID,
            call.projectedChatID,
        ])).toEqual([
            [4, 0],
            [5, 1],
        ])
    })
})
