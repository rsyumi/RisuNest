import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    session: null as any,
    currentCharacter: null as any,
    modelResponse: null as any,
    modelRequestCount: 0,
}))

vi.mock('../tokenizer', async () => (await import('./tests/sendChatTestHarness')).tokenizerModule())
vi.mock('../../lang', async () => (await import('./tests/sendChatTestHarness')).langModule())
vi.mock('../alert', async () => (await import('./tests/sendChatTestHarness')).alertModule({
    alertInput: vi.fn(),
    alertNormal: vi.fn(),
    alertSelect: vi.fn(),
}))
vi.mock('../parser/chatML', async () => (await import('./tests/sendChatTestHarness')).chatMLModule())
vi.mock('../parser/parser.svelte', async () => (await import('./tests/sendChatTestHarness')).parserModule())
vi.mock('./lorebook.svelte', async () => (await import('./tests/sendChatTestHarness')).lorebookModule())
vi.mock('../util', async () => (await import('./tests/sendChatTestHarness')).utilModule({
    findCharacterbyId: () => mocks.currentCharacter,
}))
vi.mock('./request/request', () => ({
    requestChatData: vi.fn(async (_request: unknown, purpose: string) => {
        if (purpose === 'emotion') return '|igp'
        mocks.modelRequestCount += 1
        return typeof mocks.modelResponse === 'function'
            ? mocks.modelResponse()
            : mocks.modelResponse
    }),
}))
vi.mock('./stableDiff', async () => (await import('./tests/sendChatTestHarness')).stableDiffModule({
    generateAIImage: vi.fn(),
}))
vi.mock('./scripts', async () => (await import('./tests/sendChatTestHarness')).scriptsModule())
vi.mock('./templates/templates', async () => (await import('./tests/sendChatTestHarness')).templatesModule())
vi.mock('./exampleMessages', async () => (await import('./tests/sendChatTestHarness')).exampleMessagesModule())
vi.mock('./tts', async () => (await import('./tests/sendChatTestHarness')).ttsModule())
vi.mock('./memory/supaMemory', async () => (await import('./tests/sendChatTestHarness')).supaMemoryModule())
vi.mock('./group', async () => (await import('./tests/sendChatTestHarness')).groupModule())
vi.mock('./command', () => ({ processMultiCommand: vi.fn() }))
vi.mock('./infunctions', () => ({ calcString: vi.fn(() => 0) }))
vi.mock('./memory/hypamemory', async () => (await import('./tests/sendChatTestHarness')).hypamemoryModule())
vi.mock('./embedding/addinfo', async () => (await import('./tests/sendChatTestHarness')).addinfoModule())
vi.mock('./files/inlays', async () => (await import('./tests/sendChatTestHarness')).inlaysModule({
    writeInlayImage: vi.fn(async () => ''),
}))
vi.mock('./models/modelString', async () => (await import('./tests/sendChatTestHarness')).modelStringModule())
vi.mock('../sync/multiuser', async () => (await import('./tests/sendChatTestHarness')).multiuserModule())
vi.mock('./inlayScreen', async () => (await import('./tests/sendChatTestHarness')).inlayScreenModule())
vi.mock('./transformers', async () => (await import('./tests/sendChatTestHarness')).transformersModule())
vi.mock('./memory/hanuraiMemory', async () => (await import('./tests/sendChatTestHarness')).hanuraiMemoryModule())
vi.mock('./memory/hypav2', async () => (await import('./tests/sendChatTestHarness')).hypav2Module())
vi.mock('./memory/hypav3', async () => (await import('./tests/sendChatTestHarness')).hypav3Module())
vi.mock('./scriptings', async () => (await import('./tests/sendChatTestHarness')).scriptingsModule({
    runScripted: vi.fn(),
}))
vi.mock('../model/modellist', async () => (await import('./tests/sendChatTestHarness')).modellistModule())
vi.mock('./modules', async () => (await import('./tests/sendChatTestHarness')).modulesModule({
    getModuleTriggers: () => [],
}))
vi.mock('../globalApi.svelte', async () => (await import('./tests/sendChatTestHarness')).globalApiModule())
vi.mock('../plugins/plugins.svelte', async () => (await import('./tests/sendChatTestHarness')).pluginsModule())
vi.mock('../plugins/pluginDatabaseAccess', async (importOriginal) => (await import('./tests/sendChatTestHarness')).pluginDatabaseAccessModule(importOriginal as () => Promise<Record<string, unknown>>))
vi.mock('./presetChain', async () => (await import('./tests/sendChatTestHarness')).presetChainModule())
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    assertPersistentMutationAllowed: vi.fn(),
    getPersistentStorageAuthorityEpoch: () => 0,
    getPersistentNavigationGeneration: () => 0,
    acknowledgeGenerationCompletion: vi.fn(async () => undefined),
    captureSelectedConversationTarget: () => null,
    acquireCompleteConversation: vi.fn(),
    getActiveConversationSession: () => mocks.session,
    peekActiveConversationSession: () => mocks.session,
    invalidateActiveConversationSession: () => {
        mocks.session?.invalidate()
        mocks.session = null
    },
}))

import type { character, Chat, Database, Message } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import type { triggerscript } from './triggers'
import { DBState, selectedCharID } from '../stores.svelte'
import { doingChat, sendChat } from './index.svelte'

function makeChat(messages: Message[] = [{
    role: 'user',
    data: 'hello',
    chatId: 'user-message',
}]) {
    return {
        id: 'chat-a',
        name: 'Chat A',
        note: '',
        localLore: [],
        fmIndex: -1,
        message: messages,
    } as Chat
}

function makeCharacter(chat: Chat, triggers: triggerscript[]) {
    return {
        type: 'character',
        chaId: 'character-a',
        name: 'character-a',
        chatPage: 0,
        chats: [chat],
        firstMessage: '',
        alternateGreetings: [''],
        desc: '',
        personality: '',
        scenario: '',
        bias: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: triggers,
        defaultVariables: '',
        reloadKeys: 0,
        viewScreen: 'none',
        inlayViewScreen: false,
        supaMemory: false,
    } as unknown as character
}

function installDatabase(
    triggers: triggerscript[],
    onMutation?: ConstructorParameters<typeof ActiveConversationSession>[0]['onMutation'],
) {
    const sourceChat = makeChat()
    const installedCharacter = makeCharacter(sourceChat, triggers)
    DBState.db = {
        characters: [installedCharacter],
        statics: { messages: 0 },
        botPresets: [],
        botPresetsId: 0,
        aiModel: 'test-model',
        maxContext: 8_192,
        maxResponse: 128,
        promptTemplate: [{ type: 'chat', rangeStart: 0, rangeEnd: 'end' }],
        promptSettings: {
            trimStartNewChat: true,
            sendName: false,
            sendChatAsSystem: false,
            postEndInnerFormat: '',
        },
        promptInfoInsideChat: false,
        promptTextInfoInsideChat: false,
        customPromptTemplateToggle: '',
        globalChatVariables: {},
        templateDefaultVariables: '',
        mainPrompt: '',
        additionalPrompt: '',
        globalNote: '',
        jailbreak: '',
        jailbreakToggle: false,
        chainOfThought: false,
        personaPrompt: false,
        promptPreprocess: false,
        descriptionPrefix: '',
        formatingOrder: [],
        bias: [],
        outputImageModal: false,
        rememberToolUsage: false,
        removeIncompleteResponse: false,
        streamingDisplayOptimizationMode: 'off',
        autoContinueMinTokens: 0,
        autoContinueChat: false,
        igpPrompt: '',
        notification: false,
        ttsAutoSpeech: false,
        supaModelType: 'none',
        hanuraiEnable: false,
        hypav2: false,
        hypaV3: false,
        inlayErrorResponse: false,
    } as unknown as Database
    selectedCharID.set(0)
    const currentCharacter = DBState.db.characters[0] as character
    const residentChat = currentCharacter.chats[0]
    mocks.currentCharacter = currentCharacter
    mocks.session = new ActiveConversationSession({
        characterId: currentCharacter.chaId,
        conversationId: residentChat.id!,
        conversation: residentChat,
        storeRevision: 1,
        onMutation,
    })
    return {
        chat: residentChat,
        currentCharacter,
        session: mocks.session as ActiveConversationSession,
    }
}

function streamingResponse(value: string) {
    return {
        type: 'streaming',
        result: new ReadableStream<Record<string, string>>({
            start(controller) {
                controller.enqueue({ response: value })
                controller.close()
            },
        }),
    }
}

describe('sendChat start trigger session integration', () => {
    beforeEach(() => {
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        doingChat.set(false)
        mocks.session = null
        mocks.currentCharacter = null
        mocks.modelResponse = streamingResponse('answer')
        mocks.modelRequestCount = 0
    })

    it('completes generation when a setvar start trigger commits through the active session', async () => {
        const onMutation = vi.fn()
        const { chat, session } = installDatabase([{
            comment: 'set flag on start',
            type: 'start',
            conditions: [],
            effect: [{ type: 'setvar', operator: '=', var: 'flag', value: 'on' }],
        }], onMutation)

        await expect(sendChat()).resolves.toBe(true)

        expect(chat.scriptstate).toEqual({ $flag: 'on' })
        expect(chat.message.at(-1)).toMatchObject({ role: 'char', data: 'answer' })
        expect(mocks.modelRequestCount).toBe(1)
        expect(session.matchesConversation('character-a', chat)).toBe(true)
        expect(onMutation.mock.calls.map(([event]) => event.commands)).not.toContainEqual(
            ['replace-conversation'],
        )
    })

    it('does not self-replace the conversation when the start trigger changes nothing', async () => {
        const onMutation = vi.fn()
        const { chat } = installDatabase([{
            comment: 'never passes',
            type: 'start',
            conditions: [{ type: 'value', var: 'a', value: 'b', operator: '=' }],
            effect: [{ type: 'setvar', operator: '=', var: 'flag', value: 'on' }],
        }], onMutation)

        await expect(sendChat()).resolves.toBe(true)

        expect(chat.scriptstate).toBeUndefined()
        expect(onMutation.mock.calls.map(([event]) => event.commands)).not.toContainEqual(
            ['replace-conversation'],
        )
    })
})
