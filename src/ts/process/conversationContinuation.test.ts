import { beforeEach, expect, test, vi } from 'vitest'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import type { Chat, character } from '../storage/database.svelte'
import { DBState, selectedCharID } from '../stores.svelte'

const runtime = vi.hoisted(() => ({
    unexpectedNativeRuntimeAccess: () => {
        throw new Error('Unexpected native runtime access in this test')
    },
    session: null as ActiveConversationSession | null,
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    acquireDestructiveReplacementFence: runtime.unexpectedNativeRuntimeAccess,
    capturePersistentMutationToken: runtime.unexpectedNativeRuntimeAccess,
    getPersistentDataRuntime: runtime.unexpectedNativeRuntimeAccess,
    peekActiveConversationSession: () => runtime.session,
}))
vi.mock('./modules', async (importOriginal) => ({
    ...(await importOriginal<typeof import('./modules')>()),
    getModuleTriggers: () => [],
}))
vi.mock('../tokenizer', () => ({ tokenize: vi.fn(async () => 0) }))
vi.mock('../parser/parser.svelte', () => ({
    risuChatParser: (value: string) => value,
}))
vi.mock('./command', () => ({ processMultiCommand: vi.fn() }))
vi.mock('./request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('./stableDiff', () => ({ generateAIImage: vi.fn() }))
vi.mock('./files/inlays', () => ({ writeInlayImage: vi.fn() }))

const { runTrigger } = await import('./triggers')

function fixture() {
    const chat = {
        id: 'conversation-1',
        message: [
            { role: 'user', data: 'zero', chatId: 'message-0' },
            { role: 'char', data: 'one', chatId: 'message-1' },
        ],
        scriptstate: {},
    } as Chat
    const char = {
        type: 'character',
        chaId: 'character-1',
        name: 'Character',
        chatPage: 0,
        chats: [chat],
        triggerscript: [
            {
                comment: 'ordered',
                type: 'manual',
                conditions: [],
                effect: [
                    { type: 'modifychat', index: '0', value: '' },
                    { type: 'impersonate', role: 'char', value: 'tail' },
                ],
            },
        ],
        customscript: [],
        defaultVariables: '',
        firstMessage: 'first',
        alternateGreetings: [],
        lowLevelAccess: false,
    } as unknown as character
    const session = new ActiveConversationSession({
        characterId: char.chaId,
        conversationId: chat.id!,
        conversation: chat,
        storeRevision: 19,
    })
    return { chat, char, session }
}

beforeEach(() => {
    runtime.session = null
    selectedCharID.set(0)
})

import { appendDefaultChatInput } from '../../lib/ChatScreens/defaultChatInput'
import {
    captureConversationMutationTarget,
    isConversationMutationTargetCurrent,
} from '../conversationMutations'
import { applyGenerationResponse } from './generationResponseApplication'

test.each(['setvar', 'impersonate', 'none'])(
    'input trigger own %s commit and later input append',
    async (kind) => {
        const { chat, char, session } = fixture()
        char.triggerscript[0].type = 'input'
        char.triggerscript[0].effect = (
            kind === 'setvar'
                ? [{ type: 'setvar', var: 'state', value: 'new', operator: '=' }]
                : kind === 'impersonate'
                  ? [{ type: 'impersonate', role: 'char', value: 'trigger text' }]
                  : []
        ) as never
        runtime.session = session
        DBState.db = { characters: [char], templateDefaultVariables: '' } as never
        const target = captureConversationMutationTarget(char, chat, session)
        const processInput = vi.fn(async () => 'requested input')
        const result = await appendDefaultChatInput({
            target,
            runInputTrigger: (onConversationCommit) =>
                runTrigger(char, 'input', { chat, onConversationCommit }),
            processInput,
            isTargetCurrent: (current = target) =>
                isConversationMutationTargetCurrent(current, char, chat, session),
            createMessage: (data) => ({ role: 'user', data }),
        })
        expect(result).toBe(true)
        expect(chat.message.at(-1)?.data).toBe('requested input')
        if (kind === 'setvar') expect(chat.scriptstate).toEqual({ $state: 'new' })
        if (kind === 'impersonate') expect(chat.message.at(-2)?.data).toBe('trigger text')
    },
)

test.each(['success', 'streaming'] as const)(
    '%s output triggers preserve their output through metadata, edits and appended messages',
    async (responseType) => {
        for (const action of ['metadata', 'rewrite', 'append', 'delete'] as const) {
            const { chat, char, session } = fixture()
            char.triggerscript[0].type = 'output'
            char.triggerscript[0].effect = (
                action === 'metadata'
                    ? [{ type: 'setvar', var: 'state', value: 'new', operator: '=' }]
                    : action === 'rewrite'
                      ? [{ type: 'modifychat', index: '2', value: 'rewritten' }]
                      : action === 'append'
                        ? [{ type: 'impersonate', role: 'char', value: 'added by trigger' }]
                        : [{ type: 'cutchat', start: '0', end: '2' }]
            ) as never
            runtime.session = session
            DBState.db = { characters: [char], templateDefaultVariables: '' } as never
            const listeners = vi.fn(async () => {
                if (action !== 'metadata') return
                session.acknowledgePersisted(session.sessionToken, session.version, 20)
                const persisted = structuredClone(chat)
                Object.assign(persisted.message[2], { __translation: 'record' })
                expect(session.adoptPersistedMetadata(persisted, 21)).toBe(true)
            })
            const result = await applyGenerationResponse({
                response:
                    responseType === 'success'
                        ? { type: 'success', result: 'answer' }
                        : {
                              type: 'streaming',
                              result: new ReadableStream({
                                  start(c) {
                                      c.enqueue({ response: 'answer' })
                                      c.close()
                                  },
                              }),
                          },
                abortSignal: new AbortController().signal,
                continueGeneration: false,
                sayingCharacterId: char.chaId,
                generationId: 'generation-a',
                generationInfo: {} as never,
                promptInfo: {} as never,
                removeIncompleteResponse: () => false,
                streamingDisplayOptimizationMode: () => 'off',
                ttsAutoSpeech: () => false,
                operation: {
                    getCurrentSession: () => session,
                    getTargetChat: () => chat,
                    isOwnerCurrent: () => true,
                    publishTargetChat: () => {},
                    invalidateSession: () => session.invalidate(),
                    incrementReloadKeys: () => {},
                },
                callbacks: {
                    reformatContent: (v) => v,
                    processOutput: async (data) => ({ data, emoChanged: false }),
                    runCurrentChatParser: (c) => c,
                    runInlay: (data) => ({ text: data }),
                    runOutputTrigger: (c, onConversationCommit) =>
                        runTrigger(char, 'output', { chat: c, onConversationCommit }),
                    runOutputListeners: listeners,
                    speak: async () => {},
                    trimIncompleteResponse: (v) => v,
                    markResponseApplied: () => {},
                    onProviderFailure: () => {},
                },
            })
            if (action === 'delete') {
                expect(result).toBeNull()
                expect(chat.message.map((message) => message.data)).toEqual(['zero', 'one'])
                expect(listeners).not.toHaveBeenCalled()
            } else {
                expect(result).not.toBeNull()
                expect(result!.readOutput()?.data).toBe(
                    action === 'rewrite' ? 'rewritten' : 'answer',
                )
                expect(listeners).toHaveBeenCalledWith(chat, 2)
                if (action === 'metadata') {
                    expect(chat.scriptstate).toEqual({ $state: 'new' })
                    expect(result!.readOutput()).toMatchObject({ __translation: 'record' })
                }
                if (action === 'append') expect(chat.message[3].data).toBe('added by trigger')
                result!.release()
            }
            expect(session.activePinReasons).toEqual([])
        }
    },
)

test('keeps the committed continuation as the selected multiline response candidate', async () => {
    const { chat, char, session } = fixture()
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never

    const result = await applyGenerationResponse({
        response: {
            type: 'multiline',
            result: [
                ['char', ' first'],
                ['char', 'second'],
                ['char', 'third'],
            ],
        },
        abortSignal: new AbortController().signal,
        continueGeneration: true,
        sayingCharacterId: char.chaId,
        generationId: 'generation-continue',
        generationInfo: {} as never,
        promptInfo: {} as never,
        removeIncompleteResponse: () => false,
        streamingDisplayOptimizationMode: () => 'off',
        ttsAutoSpeech: () => false,
        operation: {
            getCurrentSession: () => session,
            getTargetChat: () => chat,
            isOwnerCurrent: () => true,
            publishTargetChat: () => {},
            invalidateSession: () => session.invalidate(),
            incrementReloadKeys: () => {},
        },
        callbacks: {
            reformatContent: (value) => value,
            processOutput: async (data) => ({ data, emoChanged: false }),
            runCurrentChatParser: (current) => current,
            runInlay: (data) => ({ text: data }),
            runOutputTrigger: async () => null,
            runOutputListeners: async () => {},
            speak: async () => {},
            trimIncompleteResponse: (value) => value,
            markResponseApplied: () => {},
            onProviderFailure: () => {},
        },
    })

    expect(chat.message.at(-1)?.data).toBe('one first')
    expect(chat.message.at(-1)?.responseVariants?.candidates.map(
        (candidate) => candidate.messages[0].data,
    )).toEqual(['one first', 'second', 'third'])
    result?.release()
})
