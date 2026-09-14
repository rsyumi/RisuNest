import { describe, expect, it, vi } from 'vitest'

import { ChatRenderIdentityRegistry } from '../chatRenderIdentity'
import type { Chat, Message } from './database.svelte'
import {
    ActiveConversationSession,
    cloneConversationMetadata,
    ConversationNotFoundError,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
    MessageLocatorNotFoundError,
    requireCurrentConversationSession,
    type ActiveConversationTransaction,
    type ActiveConversationPinReason,
    type ConversationPosition,
    type MessageLocator,
} from './activeConversationSession'

function message(id: string | undefined, data: string): Message {
    return {
        role: 'user',
        data,
        ...(id === undefined ? {} : { chatId: id }),
    }
}

function chat(messages: Message[] = [
    message('duplicate', 'zero'),
    message(undefined, 'one'),
    message('duplicate', 'two'),
    message('tail', 'three'),
]): Chat {
    return {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: messages,
    }
}

function createSession(conversation = chat(), onMutation = vi.fn()) {
    return {
        conversation,
        onMutation,
        session: new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            onMutation,
        }),
    }
}

describe('ActiveConversationSession', () => {
    it('adopts persisted metadata without allowing an old message edit to overwrite it', () => {
        const { session, conversation, onMutation } = createSession()
        const locator = session.locate(3)
        const pin = session.acquirePin('transaction')
        const replacement = structuredClone(conversation)
        replacement.scriptstate = { $state: 'new' }
        Object.assign(replacement.message[3], { __translation: 'record' })
        expect(session.adoptPersistedMetadata(replacement, 8)).toBe(true)
        expect(session.canContinueGenerationFrom(locator.sessionVersion)).toBe(true)
        expect(session.ownsMessageLocator(locator)).toBe(false)
        expect(session.readMessage(session.locate(3))).toMatchObject({
            data: 'three',
            __translation: 'record',
        })
        expect(conversation.scriptstate).toEqual({ $state: 'new' })
        expect(onMutation).not.toHaveBeenCalled()
        session.append(message('later', 'new turn'))
        expect(session.canContinueGenerationFrom(locator.sessionVersion)).toBe(false)
        pin.release()
        expect(session.pinCount('transaction')).toBe(0)
    })

    it.each(['data', 'chatId', 'role', 'disabled', 'isComment', 'name'] as const)(
        'rejects persisted replacement changing message %s',
        (field) => {
            const { session, conversation } = createSession()
            const replacement = structuredClone(conversation)
            Object.assign(replacement.message[0], { [field]: 'different' })
            expect(session.adoptPersistedMetadata(replacement, 8)).toBe(false)
            expect(session.version).toBe(0)
            expect(conversation.message[0].data).toBe('zero')
        },
    )

    it('does not acknowledge pending local edits as a persisted metadata replacement', () => {
        const { session, conversation } = createSession()
        const replacement = structuredClone(conversation)
        session.append(message('local', 'unsaved'))
        expect(session.adoptPersistedMetadata(replacement, 8)).toBe(false)
        expect(conversation.message.at(-1)?.data).toBe('unsaved')
    })
    it('reads and resolves an owned message through a locator without exposing the backing array', () => {
        const { session } = createSession(chat([
            message('duplicate', 'first'),
            message('duplicate', 'second'),
            message('target-id', 'target'),
        ]))

        const locator = session.findMessageLocatorById('target-id')

        expect(locator).not.toBeNull()
        expect(session.readMessage(locator!)).toMatchObject({
            data: 'target',
            chatId: 'target-id',
        })
        expect(session.ownsMessageLocator(locator!)).toBe(true)
        expect(session.findMessageLocatorById('missing')).toBeNull()
    })

    it('finds only requested message targets in bounded pages without invalidating shared locators', () => {
        const source = Array.from({ length: 300 }, (_, index) =>
            message(`message-${index}`, `message-${index}`),
        )
        source[0].chatId = 'first-target'
        source[280].chatId = 'last-target'
        let numericReads = 0
        const messages = new Proxy(source, {
            get(target, property, receiver) {
                if (typeof property === 'string' && /^\d+$/.test(property)) numericReads += 1
                return Reflect.get(target, property, receiver)
            },
        })
        const { session } = createSession(chat(messages))
        const shared = session.locate(1)
        numericReads = 0

        const first = session.findMessageTargetsByIds(['first-target'], 'first')

        expect(first).toMatchObject([{
            absoluteIndex: 0,
            message: { chatId: 'first-target', data: 'message-0' },
        }])
        expect(numericReads).toBeLessThanOrEqual(130)
        expect(session.ownsMessageLocator(shared)).toBe(true)

        const last = session.findMessageTargetsByIds(['last-target'], 'last')
        expect(last).toMatchObject([{
            absoluteIndex: 280,
            message: { chatId: 'last-target', data: 'message-280' },
        }])
        expect(session.ownsMessageLocator(shared)).toBe(true)
    })

    it('appends and edits without cloning the full backing array or untouched messages', () => {
        const first = message('first', 'first')
        const second = message('second', 'second')
        const { conversation, session } = createSession(chat([first, second]))
        const backing = conversation.message

        const appended = session.append(message('third', 'third'))
        const edited = session.edit(appended, message('third', 'edited'))

        expect(conversation.message).toBe(backing)
        expect(conversation.message[0]).toBe(first)
        expect(conversation.message[1]).toBe(second)
        expect(session.readMessage(edited).data).toBe('edited')
    })

    it('checks backing conversation identity without exposing a compatibility array', () => {
        const { conversation, session } = createSession()

        expect(session.matchesConversation('character-a', conversation)).toBe(true)
        expect(session.matchesConversation('character-b', conversation)).toBe(false)
        expect(session.matchesConversation('character-a', chat(conversation.message))).toBe(false)
    })

    it('rejects a missing full-array conversation distinctly from a missing locator', () => {
        expect(() => new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'missing',
            conversation: null,
            storeRevision: 7,
        })).toThrow(ConversationNotFoundError)

        const { session } = createSession()
        expect(() => session.locate(99)).toThrow(MessageLocatorNotFoundError)
    })

    it('reads latest, absolute ranges, and backwards entries with stable absolute locators', () => {
        const { session } = createSession()

        expect(session.readRange(1, 2)).toEqual({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            messages: [message(undefined, 'one'), message('duplicate', 'two')],
            locators: [
                {
                    conversationId: 'conversation-a',
                    absoluteIndex: 1,
                    sessionVersion: 0,
                    sessionToken: expect.any(String),
                    locatorToken: expect.any(String),
                },
                {
                    conversationId: 'conversation-a',
                    absoluteIndex: 2,
                    expectedMessageId: 'duplicate',
                    sessionVersion: 0,
                    sessionToken: expect.any(String),
                    locatorToken: expect.any(String),
                },
            ],
            startIndex: 1,
            endIndex: 3,
            totalMessages: 4,
            storeRevision: 7,
            sessionVersion: 0,
        })
        expect(session.readLatest(2).messages.map((item) => item.data)).toEqual(['two', 'three'])
        expect(session.readRange(20, 3)).toMatchObject({
            messages: [],
            startIndex: 4,
            endIndex: 4,
            totalMessages: 4,
        })
        const backward = session.scanBackward(3, 2)
        expect(backward).toMatchObject({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            startIndexExclusive: 3,
            totalMessages: 4,
            storeRevision: 7,
            sessionVersion: 0,
        })
        expect(backward.entries.map((entry) => ({
            absoluteIndex: entry.absoluteIndex,
            data: entry.message.data,
        }))).toEqual([
            { absoluteIndex: 2, data: 'two' },
            { absoluteIndex: 1, data: 'one' },
        ])
    })

    it('returns detached message snapshots from every read API', () => {
        const messages = ['range', 'backward', 'branch'].map((id, index) => ({
            ...message(id, id),
            generationInfo: {
                stageTiming: { stage1: index + 1 },
            },
        }))
        const { conversation, session } = createSession(chat(messages))

        const range = session.readRange(0, 1)
        const backward = session.scanBackward(2, 1)
        const branch = session.readBranchSource(session.locate(2))
        range.messages[0].generationInfo!.stageTiming!.stage1 = 100
        backward.entries[0].message.generationInfo!.stageTiming!.stage1 = 200
        branch.messages[2].generationInfo!.stageTiming!.stage1 = 300

        session.transaction((transaction) => {
            const transactionRange = transaction.readRange(0, 1)
            transactionRange.messages[0].generationInfo!.stageTiming!.stage1 = 400
            transaction.append(message('append', 'append'))
        })

        expect(conversation.message.slice(0, 3).map(
            (item) => item.generationInfo?.stageTiming?.stage1,
        )).toEqual([1, 2, 3])
        expect(range.messages[0]).not.toBe(conversation.message[0])
        expect(backward.entries[0].message).not.toBe(conversation.message[1])
        expect(branch.messages[2]).not.toBe(conversation.message[2])
    })

    it('strictly validates read indices and bounded counts', () => {
        const { session } = createSession()

        for (const [start, limit] of [
            [-1, 1],
            [1.5, 1],
            [Number.POSITIVE_INFINITY, 1],
            [0, 0],
            [0, -1],
            [0, 4_097],
        ]) {
            expect(() => session.readRange(start, limit)).toThrow(RangeError)
        }
        expect(() => session.scanBackward(-1, 1)).toThrow(RangeError)
    })

    it('appends, edits, deletes, and truncates with canonical full-array results', () => {
        const { conversation, session } = createSession()

        const appended = session.append(message('append', 'four'))
        expect(appended).toEqual({
            conversationId: 'conversation-a',
            absoluteIndex: 4,
            expectedMessageId: 'append',
            sessionVersion: 1,
            sessionToken: expect.any(String),
            locatorToken: expect.any(String),
        })
        const edited = session.edit(appended, message('append', 'edited four'))
        expect(edited.sessionVersion).toBe(2)
        session.delete(session.locate(1))
        session.truncate(session.locate(2))

        expect(conversation.message).toEqual([
            message('duplicate', 'zero'),
            message('duplicate', 'two'),
        ])
        expect(session.version).toBe(4)
    })

    it('updates bookmark metadata and missing message IDs through strict locators', () => {
        const { conversation, onMutation, session } = createSession()
        const missingId = session.locate(1)

        const bookmarked = session.setBookmark(missingId, {
            bookmarked: true,
            messageId: 'assigned-id',
            name: 'Assigned bookmark',
        })

        expect(conversation.message[1].chatId).toBe('assigned-id')
        expect(conversation.bookmarks).toEqual(['assigned-id'])
        expect(conversation.bookmarkNames).toEqual({
            'assigned-id': 'Assigned bookmark',
        })
        expect(bookmarked.absoluteIndex).toBe(1)
        expect(bookmarked.sessionVersion).toBe(1)

        const renamed = session.renameBookmark(bookmarked, 'Renamed bookmark')
        expect(conversation.bookmarkNames).toEqual({
            'assigned-id': 'Renamed bookmark',
        })

        session.setBookmark(renamed, { bookmarked: false })
        expect(conversation.bookmarks).toEqual([])
        expect(conversation.bookmarkNames).toEqual({})
        expect(conversation.message[1].chatId).toBe('assigned-id')
        expect(onMutation.mock.calls.map(([event]) => event.commands)).toEqual([
            ['bookmark'],
            ['bookmark'],
            ['bookmark'],
        ])
    })

    it('preserves duplicate bookmark IDs and removes their first bookmark occurrence', () => {
        const conversation = chat()
        conversation.bookmarks = ['duplicate', 'duplicate']
        conversation.bookmarkNames = { duplicate: 'Shared name' }
        const { session } = createSession(conversation)

        session.setBookmark(session.locate(2), { bookmarked: false })

        expect(conversation.bookmarks).toEqual(['duplicate'])
        expect(conversation.bookmarkNames).toEqual({})
        expect(conversation.message[0].chatId).toBe('duplicate')
        expect(conversation.message[2].chatId).toBe('duplicate')
    })

    it('never uses a supplied message ID to remove another bookmark', () => {
        const conversation = chat()
        conversation.bookmarks = ['duplicate']
        conversation.bookmarkNames = { duplicate: 'Duplicate' }
        const { session } = createSession(conversation)

        const unchanged = session.setBookmark(session.locate(1), {
            bookmarked: false,
            messageId: 'duplicate',
        })

        expect(conversation.bookmarks).toEqual(['duplicate'])
        expect(conversation.bookmarkNames).toEqual({ duplicate: 'Duplicate' })
        expect(conversation.message[1].chatId).toBeUndefined()
        expect(unchanged.sessionVersion).toBe(0)
        expect(session.version).toBe(0)
    })

    it.each([
        ['null', null],
        ['empty', ''],
    ])('assigns a usable ID when bookmarking a message with a %s ID', (_label, chatId) => {
        const conversation = chat()
        conversation.message[1].chatId = chatId as unknown as string
        const { session } = createSession(conversation)

        session.setBookmark(session.locate(1), {
            bookmarked: true,
            messageId: 'assigned-id',
        })

        expect(conversation.message[1].chatId).toBe('assigned-id')
        expect(conversation.bookmarks).toEqual(['assigned-id'])
        session.setBookmark(session.locate(1), { bookmarked: false })
        expect(conversation.bookmarks).toEqual([])
    })

    it('does not replace bookmark metadata identities for ordinary message commands', () => {
        const conversation = chat()
        conversation.bookmarks = ['duplicate']
        conversation.bookmarkNames = { duplicate: 'Duplicate' }
        const originalBookmarks = conversation.bookmarks
        const originalBookmarkNames = conversation.bookmarkNames
        const { session } = createSession(conversation)

        session.edit(session.locate(0), message('duplicate', 'edited'))

        expect(conversation.bookmarks).toBe(originalBookmarks)
        expect(conversation.bookmarkNames).toBe(originalBookmarkNames)
    })

    it('resolves exact live locators and assigns a missing ID only once through that locator', () => {
        const { conversation, session } = createSession(chat([
            message('', 'empty-id'),
            message('existing', 'existing-id'),
        ]))
        const emptyLocator = session.locate(0)
        const createId = vi.fn(() => 'generated-id')

        const first = session.ensureMessageId(emptyLocator, createId)
        const second = session.ensureMessageId(emptyLocator, createId)

        expect(first).toBe(conversation.message[0])
        expect(second).toBe(first)
        expect(first.chatId).toBe('generated-id')
        expect(createId).toHaveBeenCalledOnce()
        expect(session.version).toBe(0)

        session.edit(session.locate(1), message('existing', 'changed elsewhere'))
        expect(() => session.resolveMessage(emptyLocator)).toThrow(ConversationSessionStaleError)
    })

    it('assigns nullish message IDs as exact contiguous edit ranges', () => {
        const first = message(undefined, 'first')
        const empty = message('', 'empty remains compatible')
        const existing = message('existing', 'existing')
        const last = message(undefined, 'last')
        const { conversation, onMutation, session } = createSession(chat([
            first,
            empty,
            existing,
            last,
        ]))
        const createId = vi.fn()
            .mockReturnValueOnce('generated-first')
            .mockReturnValueOnce('generated-last')

        expect(session.ensureNullishMessageIds(createId)).toBe(2)

        expect(conversation.message.map((entry) => entry.chatId)).toEqual([
            'generated-first',
            '',
            'existing',
            'generated-last',
        ])
        expect(conversation.message[1]).toBe(empty)
        expect(conversation.message[2]).toBe(existing)
        expect(session.version).toBe(2)
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['edit', 'edit'],
            mutations: [
                expect.objectContaining({ start: 0, deleteCount: 1, sessionVersion: 1 }),
                expect.objectContaining({ start: 3, deleteCount: 1, sessionVersion: 2 }),
            ],
        }))
    })

    it('atomically commits disjoint message ranges and metadata from one expected version', () => {
        const { conversation, onMutation, session } = createSession()
        const untouchedOne = conversation.message[1]
        const untouchedTwo = conversation.message[2]
        const expectedMetadata = cloneConversationMetadata(conversation)
        const firstPosition = session.positionAt(0)
        const lastPosition = session.positionAt(3)

        session.applyOperation({
            expectedVersion: 0,
            expectedMetadata,
            metadata: {
                ...expectedMetadata,
                name: 'Parsed conversation',
                scriptstate: { '$counter': '2' },
            },
            ranges: [
                {
                    position: firstPosition,
                    deleteCount: 1,
                    expectedMessages: [message('duplicate', 'zero')],
                    messages: [message('duplicate', 'parsed zero')],
                },
                {
                    position: lastPosition,
                    deleteCount: 1,
                    expectedMessages: [message('tail', 'three')],
                    messages: [message('tail', 'parsed three')],
                },
            ],
        })

        expect(conversation.message.map((entry) => entry.data)).toEqual([
            'parsed zero',
            'one',
            'two',
            'parsed three',
        ])
        expect(conversation.message[1]).toBe(untouchedOne)
        expect(conversation.message[2]).toBe(untouchedTwo)
        expect(conversation.name).toBe('Parsed conversation')
        expect(conversation.scriptstate).toEqual({ '$counter': '2' })
        expect(session.version).toBe(2)
        expect(onMutation).toHaveBeenCalledOnce()
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            previousVersion: 0,
            sessionVersion: 2,
            commands: ['replace-range', 'replace-range', 'update-metadata'],
            mutations: [
                expect.objectContaining({
                    start: 0,
                    deleteCount: 1,
                    sessionVersion: 1,
                }),
                expect.objectContaining({
                    start: 3,
                    deleteCount: 1,
                    sessionVersion: 2,
                }),
            ],
            conversation: expect.objectContaining({
                name: 'Parsed conversation',
                scriptstate: { '$counter': '2' },
            }),
        }))
    })

    it('rejects a detached multi-range commit after its message baseline changes in place', () => {
        const { conversation, onMutation, session } = createSession()
        const expectedMetadata = cloneConversationMetadata(conversation)
        const expectedMessage = session.readRange(0, 1).messages
        const position = session.positionAt(0)
        conversation.message[0].data = 'compatibility mutation'

        expect(() => session.applyOperation({
            expectedVersion: 0,
            expectedMetadata,
            metadata: expectedMetadata,
            ranges: [{
                position,
                deleteCount: 1,
                expectedMessages: expectedMessage,
                messages: [message('duplicate', 'parsed zero')],
            }],
        })).toThrow(MessageLocatorMismatchError)

        expect(conversation.message[0].data).toBe('compatibility mutation')
        expect(session.version).toBe(0)
        expect(onMutation).not.toHaveBeenCalled()
    })

    it('rolls back every disjoint range and metadata field when publication fails', () => {
        const conversation = chat()
        const originalMessages = conversation.message
        const onMutation = vi.fn(() => {
            throw new Error('publication failed')
        })
        const { session } = createSession(conversation, onMutation)
        const expectedMetadata = cloneConversationMetadata(conversation)

        expect(() => session.applyOperation({
            expectedVersion: 0,
            expectedMetadata,
            metadata: { ...expectedMetadata, scriptstate: { '$counter': '2' } },
            ranges: [
                {
                    position: session.positionAt(0),
                    deleteCount: 1,
                    messages: [message('duplicate', 'parsed zero')],
                },
                {
                    position: session.positionAt(3),
                    deleteCount: 1,
                    messages: [message('tail', 'parsed three')],
                },
            ],
        })).toThrow('publication failed')

        expect(conversation.message).toBe(originalMessages)
        expect(conversation.message.map((entry) => entry.data)).toEqual([
            'zero',
            'one',
            'two',
            'three',
        ])
        expect(conversation.scriptstate).toBeUndefined()
        expect(session.version).toBe(0)
    })

    it('adopts a trigger replacement only against the captured version and message array', () => {
        const { conversation, onMutation, session } = createSession()
        const expectedMessages = conversation.message
        const replacementMessages = [message('triggered', 'triggered')]
        const replacement: Chat = {
            id: 'conversation-a',
            name: 'Triggered',
            note: '',
            localLore: [],
            message: replacementMessages,
        }
        const staleLocator = session.locate(0)

        const adopted = session.adoptConversationReplacement(0, expectedMessages, replacement)

        expect(adopted).toBe(conversation)
        expect(conversation).toEqual(replacement)
        expect(conversation.message).toBe(replacementMessages)
        expect(session.version).toBe(1)
        expect(() => session.resolveMessage(staleLocator)).toThrow(ConversationSessionStaleError)
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            previousVersion: 0,
            sessionVersion: 1,
            commands: ['replace-conversation'],
        }))

        const currentMessages = conversation.message
        session.edit(session.locate(0), message('triggered', 'concurrent edit'))
        expect(() => session.adoptConversationReplacement(1, currentMessages, replacement))
            .toThrow(ConversationSessionStaleError)
        expect(conversation.message[0].data).toBe('concurrent edit')
        expect(conversation.name).toBe('Triggered')
    })

    it('replaces tails for reroll and exposes an inclusive branch source without cloning the chat shape', () => {
        const { conversation, session } = createSession()

        session.replaceTail(session.positionAt(2), [
            message('replacement-a', 'replacement two'),
            message('replacement-b', 'replacement three'),
        ])
        session.reroll(session.positionAt(3), [message('rerolled', 'rerolled three')])

        expect(conversation.message).toEqual([
            message('duplicate', 'zero'),
            message(undefined, 'one'),
            message('replacement-a', 'replacement two'),
            message('rerolled', 'rerolled three'),
        ])
        expect(session.readBranchSource(session.locate(2))).toEqual({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            messages: conversation.message.slice(0, 3),
            startIndex: 0,
            endIndex: 3,
            totalMessages: 4,
            storeRevision: 7,
            sessionVersion: 2,
        })
    })

    it('fails stale, shifted, mismatched, and replaced locators instead of retargeting', () => {
        const { session } = createSession()
        const shifted = session.locate(2)
        const replaced = session.locate(3)
        const wrongConversation = { ...session.locate(0), conversationId: 'conversation-b' }

        session.delete(session.locate(0))
        expect(() => session.edit(shifted, message('duplicate', 'wrong target'))).toThrow(
            ConversationSessionStaleError,
        )
        expect(() => session.edit(replaced, message('tail', 'wrong replacement'))).toThrow(
            ConversationSessionStaleError,
        )
        expect(() => session.edit(wrongConversation, message('duplicate', 'wrong conversation'))).toThrow(
            MessageLocatorMismatchError,
        )

        const current = session.locate(2)
        session.edit(current, message('tail', 'replacement with same ID'))
        expect(() => session.edit(current, message('tail', 'stale replacement'))).toThrow(
            ConversationSessionStaleError,
        )
    })

    it('rejects locators after direct replacement even when IDs cannot detect retargeting', () => {
        const duplicateCase = createSession()
        const duplicate = duplicateCase.session.locate(0)
        duplicateCase.conversation.message[0] = duplicateCase.conversation.message[2]
        expect(() => duplicateCase.session.edit(
            duplicate,
            message('duplicate', 'must not retarget duplicate'),
        )).toThrow(MessageLocatorMismatchError)

        const missingCase = createSession()
        const missing = missingCase.session.locate(1)
        missingCase.conversation.message[1] = message(undefined, 'different missing ID')
        expect(() => missingCase.session.delete(missing)).toThrow(MessageLocatorMismatchError)

        const sameIdCase = createSession()
        const sameId = sameIdCase.session.locate(3)
        sameIdCase.conversation.message[3] = message('tail', 'different object with same ID')
        expect(() => sameIdCase.session.edit(
            sameId,
            message('tail', 'must not retarget replacement'),
        )).toThrow(MessageLocatorMismatchError)

        const arrayCase = createSession()
        const arrayLocator = arrayCase.session.locate(0)
        const arrayPosition = arrayCase.session.positionAt(2)
        arrayCase.conversation.message = arrayCase.conversation.message.slice()
        expect(() => arrayCase.session.delete(arrayLocator)).toThrow(MessageLocatorMismatchError)
        expect(() => arrayCase.session.replaceTail(arrayPosition, [])).toThrow(
            MessageLocatorMismatchError,
        )
    })

    it('validates structural locator tokens across clone, spread, and proxy transport', () => {
        const cloneCase = createSession()
        const cloneLocator = cloneCase.session.locate(0)
        expect(typeof cloneLocator.sessionToken).toBe('string')
        expect(typeof cloneLocator.locatorToken).toBe('string')
        expect(() => cloneCase.session.edit(
            structuredClone(cloneLocator),
            message('duplicate', 'cloned locator'),
        )).not.toThrow()

        const spreadCase = createSession()
        const spreadLocator = { ...spreadCase.session.locate(1) }
        expect(() => spreadCase.session.delete(spreadLocator)).not.toThrow()

        const proxyCase = createSession()
        const proxyLocator = new Proxy(proxyCase.session.locate(2), {})
        expect(() => proxyCase.session.edit(
            proxyLocator,
            message('duplicate', 'proxied locator'),
        )).not.toThrow()

        const positionCase = createSession()
        const clonedPosition = structuredClone(positionCase.session.positionAt(2))
        expect(typeof clonedPosition.positionToken).toBe('string')
        expect(() => positionCase.session.replaceTail(clonedPosition, [])).not.toThrow()

        const forgedCase = createSession()
        const valid = forgedCase.session.locate(0)
        const forgedLocator = {
            ...valid,
            locatorToken: 'forged-locator-token',
        } as MessageLocator
        const forgedSession = {
            ...valid,
            sessionToken: 'forged-session-token',
        } as MessageLocator
        const forgedPosition = {
            ...forgedCase.session.positionAt(1),
            positionToken: 'forged-position-token',
        } as ConversationPosition
        expect(() => forgedCase.session.delete(forgedLocator)).toThrow(
            MessageLocatorMismatchError,
        )
        expect(() => forgedCase.session.delete(forgedSession)).toThrow(
            MessageLocatorMismatchError,
        )
        expect(() => forgedCase.session.replaceTail(forgedPosition, [])).toThrow(
            MessageLocatorMismatchError,
        )

        const staleCase = createSession()
        const stale = structuredClone(staleCase.session.locate(0))
        staleCase.session.append(message('new', 'version advance'))
        expect(() => staleCase.session.delete(stale)).toThrow(ConversationSessionStaleError)
    })

    it('keeps a same-version locator valid after reading the maximum range', () => {
        const messages = Array.from({ length: 4_096 }, (_value, index) =>
            message(`message-${index}`, `message ${index}`),
        )
        const { conversation, session } = createSession(chat(messages))
        const first = session.locate(0)

        session.readRange(0, 4_096)
        session.edit(first, message('message-0', 'edited message 0'))

        expect(conversation.message[0].data).toBe('edited message 0')
    })

    it('reuses locator tokens across large repeated reads in one version', () => {
        const messages = Array.from({ length: 4_096 }, (_value, index) =>
            message(`message-${index}`, `message ${index}`),
        )
        const { session } = createSession(chat(messages))
        const observedTokens = new Set<string>()
        let firstReadTokens: string[] | null = null

        for (let read = 0; read < 8; read++) {
            const tokens = session.readRange(0, 4_096).locators.map(
                (locator) => locator.locatorToken,
            )
            for (const token of tokens) observedTokens.add(token)
            firstReadTokens ??= tokens
            expect(tokens.every(
                (token, index) => token === firstReadTokens[index],
            )).toBe(true)
        }

        expect(observedTokens.size).toBe(4_096)
    })

    it('publishes a successful transaction once and rolls back the whole draft after any failure', () => {
        const { conversation, onMutation, session } = createSession()
        const original = structuredClone(conversation.message)
        const staleInsideTransaction = session.locate(2)

        expect(() => session.transaction((transaction) => {
            transaction.delete(transaction.locate(0))
            transaction.edit(staleInsideTransaction, message('duplicate', 'must not publish'))
        })).toThrow(ConversationSessionStaleError)
        expect(conversation.message).toEqual(original)
        expect(session.version).toBe(0)
        expect(onMutation).not.toHaveBeenCalled()

        session.transaction((transaction) => {
            transaction.edit(transaction.locate(0), message('duplicate', 'edited zero'))
            transaction.append(message('append', 'four'))
        })
        expect(conversation.message.map((item) => item.data)).toEqual([
            'edited zero',
            'one',
            'two',
            'three',
            'four',
        ])
        expect(session.version).toBe(2)
        expect(onMutation).toHaveBeenCalledOnce()
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            previousVersion: 0,
            sessionVersion: 2,
            commands: ['edit', 'append'],
        }))
    })

    it('publishes detached strict replacement evidence for every committed command version', () => {
        const { conversation, onMutation, session } = createSession()

        session.transaction((transaction) => {
            transaction.edit(transaction.locate(0), message('duplicate', 'edited zero'))
            transaction.append(message('append', 'four'))
        })

        const event = onMutation.mock.calls[0][0]
        expect(event).toMatchObject({
            previousVersion: 0,
            sessionVersion: 2,
            commands: ['edit', 'append'],
            mutations: [
                {
                    start: 0,
                    deleteCount: 1,
                    messages: [message('duplicate', 'edited zero')],
                    sessionVersion: 1,
                },
                {
                    start: 4,
                    deleteCount: 0,
                    messages: [message('append', 'four')],
                    sessionVersion: 2,
                },
            ],
            conversation: {
                id: 'conversation-a',
                name: 'Conversation A',
                note: '',
                localLore: [],
            },
        })
        expect(event.sessionToken).toBeTruthy()

        event.mutations[0].messages[0].data = 'retained event mutation'
        event.conversation.name = 'retained metadata mutation'
        expect(conversation.message[0].data).toBe('edited zero')
        expect(conversation.name).toBe('Conversation A')
    })

    it('advances persisted revision only through an owned monotonic acknowledgement', () => {
        const { onMutation, session } = createSession()
        session.append(message('append', 'four'))
        const event = onMutation.mock.calls[0][0]

        expect(session.persistedVersion).toBe(0)
        expect(session.acknowledgePersisted(event.sessionToken, 1, 8)).toBe(true)
        expect(session.persistedVersion).toBe(1)
        expect(session.storeRevision).toBe(8)
        expect(session.acknowledgePersisted(event.sessionToken, 1, 8)).toBe(false)

        const other = createSession().session
        other.append(message('other', 'other'))
        const otherToken = other.locate(0).sessionToken
        expect(() => session.acknowledgePersisted(otherToken, 1, 9)).toThrow(/session/i)
        expect(() => session.acknowledgePersisted(event.sessionToken, 2, 9)).toThrow(/version/i)
        expect(() => session.acknowledgePersisted(event.sessionToken, 1, 7)).not.toThrow()
        expect(session.storeRevision).toBe(8)
    })

    it('represents metadata-only versions with a strict no-op tail range', () => {
        const conversation = chat()
        conversation.bookmarks = ['tail']
        conversation.bookmarkNames = { tail: 'Before' }
        const { onMutation, session } = createSession(conversation)

        session.renameBookmark(session.locate(3), 'After')

        expect(onMutation.mock.calls[0][0]).toMatchObject({
            previousVersion: 0,
            sessionVersion: 1,
            mutations: [{
                start: 4,
                deleteCount: 0,
                messages: [],
                sessionVersion: 1,
            }],
            conversation: {
                bookmarks: ['tail'],
                bookmarkNames: { tail: 'After' },
            },
        })
    })

    it('preserves untouched legacy message identities across edits and appends', () => {
        const originalMessages = [
            message(undefined, 'zero'),
            message(undefined, 'one'),
            message(undefined, 'two'),
        ]
        const { conversation, session } = createSession(chat(originalMessages))
        const registry = new ChatRenderIdentityRegistry()
        const before = registry.register('conversation-a', conversation.message).toArray()

        session.transaction((transaction) => {
            transaction.edit(transaction.locate(0), message(undefined, 'edited zero'))
            transaction.append(message(undefined, 'three'))
        })
        const after = registry.register('conversation-a', conversation.message).toArray()

        expect(conversation.message[1]).toBe(originalMessages[1])
        expect(conversation.message[2]).toBe(originalMessages[2])
        expect(after[1]).toBe(before[1])
        expect(after[2]).toBe(before[2])
    })

    it('restores untouched nested state when the mutation observer throws', () => {
        const edited = message('edited', 'zero')
        edited.generationInfo = { stageTiming: { stage1: 1 } }
        const untouched = message('untouched', 'one')
        untouched.generationInfo = { stageTiming: { stage1: 2 } }
        const originalMessages = [edited, untouched]
        const conversation = chat(originalMessages)
        const onMutation = vi.fn(() => {
            conversation.message[1].generationInfo!.stageTiming!.stage1 = 99
            throw new Error('observer failed')
        })
        const { session } = createSession(conversation, onMutation)

        expect(() => session.edit(
            session.locate(0),
            message('edited', 'replacement'),
        )).toThrow('observer failed')

        expect(conversation.message).toBe(originalMessages)
        expect(conversation.message[1]).toBe(untouched)
        expect(conversation.message[1].generationInfo?.stageTiming?.stage1).toBe(2)
        expect(session.version).toBe(0)
    })

    it('isolates nested draft message state when a transaction throws', () => {
        const nested = message('nested', 'original')
        nested.generationInfo = {
            stageTiming: { stage1: 1 },
        }
        nested.promptInfo = {
            promptToggles: [{ key: 'mode', value: 'original' }],
        }
        const { conversation, onMutation, session } = createSession(chat([nested]))
        const originalArray = conversation.message

        expect(() => session.transaction((transaction) => {
            transaction.messages[0].generationInfo!.stageTiming!.stage1 = 99
            transaction.messages[0].promptInfo!.promptToggles![0].value = 'changed'
            throw new Error('abort nested changes')
        })).toThrow('abort nested changes')

        expect(conversation.message).toBe(originalArray)
        expect(conversation.message[0].generationInfo?.stageTiming?.stage1).toBe(1)
        expect(conversation.message[0].promptInfo?.promptToggles?.[0].value).toBe('original')
        expect(session.version).toBe(0)
        expect(onMutation).not.toHaveBeenCalled()
    })

    it('does not expose committed state through retained drafts or command inputs', () => {
        const original = message('nested', 'original')
        original.generationInfo = { stageTiming: { stage1: 1 } }
        const appended = message('append', 'appended')
        appended.generationInfo = { stageTiming: { stage1: 2 } }
        const { conversation, session } = createSession(chat([original]))
        let retainedDraft!: Message[]
        let retainedTransaction!: ActiveConversationTransaction

        session.transaction((transaction) => {
            retainedTransaction = transaction
            retainedDraft = transaction.messages
            transaction.append(appended)
        })

        retainedDraft[0].generationInfo!.stageTiming!.stage1 = 100
        appended.generationInfo!.stageTiming!.stage1 = 200

        expect(conversation.message.map(
            (item) => item.generationInfo?.stageTiming?.stage1,
        )).toEqual([1, 2])
        expect(() => retainedTransaction.append(message('late', 'late'))).toThrow(/closed/)
    })

    it('rejects async callbacks without publishing mutations before or after await', async () => {
        const { conversation, onMutation, session } = createSession()
        const original = structuredClone(conversation.message)
        let release!: () => void
        const gate = new Promise<void>((resolve) => {
            release = resolve
        })
        let callbackResult!: Promise<void>
        let afterAwaitError: unknown
        let thrown: unknown

        try {
            session.transaction((transaction) => {
                callbackResult = (async () => {
                    transaction.edit(
                        transaction.locate(0),
                        message('duplicate', 'before await'),
                    )
                    await gate
                    try {
                        transaction.append(message('late', 'after await'))
                    } catch (error) {
                        afterAwaitError = error
                    }
                    throw new Error('late transaction rejection')
                })()
                return callbackResult
            })
        } catch (error) {
            thrown = error
        }

        expect(thrown).toBeInstanceOf(TypeError)
        expect(conversation.message).toEqual(original)
        expect(session.version).toBe(0)
        expect(onMutation).not.toHaveBeenCalled()

        release()
        await expect(callbackResult).rejects.toThrow('late transaction rejection')
        expect(afterAwaitError).toBeInstanceOf(Error)
        expect((afterAwaitError as Error).message).toContain('closed')
        expect(conversation.message).toEqual(original)
        expect(session.version).toBe(0)
        expect(onMutation).not.toHaveBeenCalled()
    })

    it('rolls back the published array and version when mutation notification throws', () => {
        const conversation = chat()
        const originalArray = conversation.message
        const original = structuredClone(originalArray)
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            onMutation: () => {
                throw new Error('mutation observer failed')
            },
        })

        expect(() => session.edit(
            session.locate(0),
            message('duplicate', 'must roll back'),
        )).toThrow('mutation observer failed')

        expect(conversation.message).toBe(originalArray)
        expect(conversation.message).toEqual(original)
        expect(session.version).toBe(0)
    })

    it('rejects observer reentry during append and rolls the outer append back', () => {
        const conversation = chat()
        const originalArray = conversation.message
        const original = structuredClone(originalArray)
        let session!: ActiveConversationSession
        let reenter = true
        session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            onMutation: () => {
                if (!reenter) return
                reenter = false
                session.edit(session.locate(0), message('duplicate', 'observer edit'))
            },
        })

        expect(() => session.append(message('append', 'outer append'))).toThrow(
            /Nested conversation session transactions/,
        )
        expect(conversation.message).toBe(originalArray)
        expect(conversation.message).toEqual(original)
        expect(session.version).toBe(0)

        const appended = session.append(message('append', 'after rollback'))
        expect(session.readMessage(appended).data).toBe('after rollback')
    })

    it('rejects observer reentry during edit and rolls the outer edit back', () => {
        const conversation = chat()
        const originalArray = conversation.message
        const original = structuredClone(originalArray)
        let session!: ActiveConversationSession
        let reenter = true
        session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            onMutation: () => {
                if (!reenter) return
                reenter = false
                session.append(message('observer', 'observer append'))
            },
        })

        expect(() => session.edit(
            session.locate(0),
            message('duplicate', 'outer edit'),
        )).toThrow(/Nested conversation session transactions/)
        expect(conversation.message).toBe(originalArray)
        expect(conversation.message).toEqual(original)
        expect(session.version).toBe(0)

        const edited = session.edit(session.locate(0), message('duplicate', 'after rollback'))
        expect(session.readMessage(edited).data).toBe('after rollback')
    })

    it('requires an awaited consumer to reacquire the same active session', () => {
        const expected = createSession().session
        const replacement = createSession().session

        expect(requireCurrentConversationSession(expected, expected)).toBe(expected)
        expect(() => requireCurrentConversationSession(expected, replacement)).toThrow(/inactive/)
        expected.invalidate()
        expect(() => requireCurrentConversationSession(expected, expected)).toThrow(/inactive/)
        expect(() => requireCurrentConversationSession(expected, null)).toThrow(/inactive/)
    })

    it('tracks the C1 pin reason contract without enabling eviction', () => {
        const { session } = createSession()
        const reasons: ActiveConversationPinReason[] = [
            'dirty',
            'pending-save',
            'streaming',
            'transaction',
            'prompt',
            'compatibility',
        ]
        const pins = reasons.map((reason) => session.acquirePin(reason))
        const secondDirty = session.acquirePin('dirty')

        expect('evictionEnabled' in session).toBe(false)
        expect(session.activePinReasons).toEqual(reasons)
        expect(session.pinCount('dirty')).toBe(2)
        pins[0].release()
        pins[0].release()
        expect(session.pinCount('dirty')).toBe(1)
        secondDirty.release()
        expect(session.activePinReasons).toEqual(reasons.slice(1))
    })

    it('bounds detached absolute interval residency and evicts least-recent unpinned ranges', () => {
        const messages = Array.from(
            { length: 10 },
            (_, index) => message(`message-${index}`, `message-${index}`),
        )
        const conversation = chat(messages)
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            maxResidentBytes: 2,
            measureMessage: () => 1,
        })

        expect(session.readRange(0, 2).messages).toEqual(messages.slice(0, 2))
        expect(session.residentIntervals).toMatchObject([{
            startIndex: 0,
            endIndex: 2,
            byteSize: 2,
        }])

        expect(session.readRange(8, 2).messages).toEqual(messages.slice(8))
        expect(session.residentBytes).toBe(2)
        expect(session.residentIntervals).toMatchObject([{
            startIndex: 8,
            endIndex: 10,
            byteSize: 2,
        }])
    })

    it('keeps counted absolute range pins resident until their idempotent release', () => {
        const messages = Array.from(
            { length: 5 },
            (_, index) => message(`message-${index}`, `message-${index}`),
        )
        const conversation = chat(messages)
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            maxResidentBytes: 1,
            measureMessage: () => 1,
        })
        const first = session.acquireRangePin(0, 1, 'viewport')
        const second = session.acquireRangePin(0, 1, 'viewport')

        session.readRange(0, 1)
        session.readRange(4, 1)

        expect(session.pinCount('viewport')).toBe(2)
        expect(session.residentIntervals).toMatchObject([{
            startIndex: 0,
            endIndex: 1,
        }])

        first.release()
        first.release()
        expect(session.pinCount('viewport')).toBe(1)
        second.release()
        expect(session.pinCount('viewport')).toBe(0)

        session.readRange(4, 1)
        expect(session.residentIntervals).toMatchObject([{
            startIndex: 4,
            endIndex: 5,
        }])
    })

    it('retains dirty and pending-save payloads across failure until exact acknowledgement', () => {
        const conversation = chat([])
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            maxResidentBytes: 0,
            measureMessage: () => 1,
        })

        session.append(message('dirty', 'dirty'))
        const failed = session.beginPersistence(1)

        expect(session.pinCount('dirty')).toBe(1)
        expect(session.pinCount('pending-save')).toBe(1)
        expect(session.residentBytes).toBe(1)

        failed.release()
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.pinCount('dirty')).toBe(1)
        expect(session.residentBytes).toBe(1)

        const retry = session.beginPersistence(1)
        retry.acknowledge(8)
        retry.acknowledge(8)

        expect(session.storeRevision).toBe(8)
        expect(session.persistedVersion).toBe(1)
        expect(session.pinCount('dirty')).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.residentBytes).toBe(0)
    })

    it('accepts ordered session acknowledgements covered by one store revision', () => {
        const { session } = createSession(chat([]))
        const first = session.append(message('first', 'first'))
        session.edit(first, message('first', 'edited'))
        const sessionToken = first.sessionToken

        expect(session.acknowledgePersisted(sessionToken, 1, 8)).toBe(true)
        expect(() => session.acknowledgePersisted(sessionToken, 2, 8)).not.toThrow()
        expect(session.persistedVersion).toBe(2)
        expect(session.storeRevision).toBe(8)
        expect(session.pendingMutations).toEqual([])
    })

    it('falls back to the complete owner when an untracked structural mutation precedes a command', () => {
        const { conversation, onMutation, session } = createSession(chat([
            message('first', 'first'),
        ]))
        conversation.message.push(message('legacy', 'legacy direct append'))

        expect(() => session.append(message('session', 'session append'))).not.toThrow()

        expect(conversation.message.map((entry) => entry.data)).toEqual([
            'first',
            'legacy direct append',
            'session append',
        ])
        expect(onMutation).toHaveBeenCalledOnce()
        expect(session.residencyFallbackActive).toBe(true)
        const event = onMutation.mock.calls[0][0]
        expect(session.acknowledgePersisted(event.sessionToken, 1, 8)).toBe(true)
        expect(session.persistedVersion).toBe(1)
        expect(session.materializeCompatibilityArray()).toBe(conversation.message)
        expect('evictionEnabled' in session).toBe(false)
    })

    it('emits a complete ordered fallback event for a multi-command transaction', () => {
        const { conversation, onMutation, session } = createSession(chat([
            message('first', 'first'),
        ]))
        conversation.message.push(message('legacy', 'legacy direct append'))

        session.transaction((transaction) => {
            transaction.append(message('second', 'second'))
            transaction.append(message('third', 'third'))
        })

        expect(onMutation).toHaveBeenCalledOnce()
        expect(onMutation.mock.calls[0][0].mutations).toEqual([
            {
                start: 0,
                deleteCount: 1,
                messages: conversation.message,
                sessionVersion: 1,
                completeOwner: true,
            },
            {
                start: 4,
                deleteCount: 0,
                messages: [],
                sessionVersion: 2,
            },
        ])
        expect(session.residencyFallbackActive).toBe(true)
    })

    it('preserves complete reads when resident measurement rejects a cache entry', () => {
        const conversation = chat([message('first', 'first')])
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            maxResidentBytes: 1,
            measureMessage: () => -1,
        })

        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        try {
            expect(() => session.readRange(0, 1)).not.toThrow()
            expect(session.readRange(0, 1).messages).toEqual(conversation.message)
            expect(session.residencyFallbackActive).toBe(true)
            expect(session.residentIntervals).toEqual([])
            expect(session.materializeCompatibilityArray()).toBe(conversation.message)
            expect(consoleError).toHaveBeenCalledWith(
                'Active conversation residency failed; using compatibility fallback',
                expect.any(RangeError),
            )
        } finally {
            consoleError.mockRestore()
        }
    })

    it('materializes a detached complete compatibility snapshot and releases it to budget', () => {
        const messages = Array.from(
            { length: 10 },
            (_, index) => message(`message-${index}`, `message-${index}`),
        )
        const conversation = chat(messages)
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            maxResidentBytes: 2,
            measureMessage: () => 1,
        })
        session.readLatest(2)

        const snapshot = session.materializeCompatibilitySnapshot()

        expect(snapshot.messages).toEqual(messages)
        expect(snapshot.messages).not.toBe(conversation.message)
        expect(snapshot.residentMessageCount).toBe(10)
        expect(session.pinCount('compatibility')).toBe(1)
        expect(session.residentBytes).toBe(10)

        snapshot.messages[0].data = 'detached mutation'
        expect(conversation.message[0].data).toBe('message-0')

        snapshot.dispose()
        snapshot.dispose()

        expect(snapshot.residentMessageCount).toBe(0)
        expect(session.pinCount('compatibility')).toBe(0)
        expect(session.residentBytes).toBe(2)
        expect(session.residentIntervals).toHaveLength(1)
        expect(session.residentIntervals[0].endIndex -
            session.residentIntervals[0].startIndex).toBe(2)
        expect('evictionEnabled' in session).toBe(false)
        expect(session.materializeCompatibilityArray()).toBe(conversation.message)
    })

    it('transfers a detached compatibility snapshot without clearing its messages', () => {
        const conversation = chat([
            message('first', 'first'),
            message('second', 'second'),
        ])
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            maxResidentBytes: 0,
            measureMessage: () => 1,
        })
        const snapshot = session.materializeCompatibilitySnapshot()

        const transferred = snapshot.takeMessages()
        snapshot.dispose()

        expect(transferred).toEqual(conversation.message)
        expect(transferred).not.toBe(conversation.message)
        expect(snapshot.residentMessageCount).toBe(0)
        expect(session.pinCount('compatibility')).toBe(0)
    })
})
