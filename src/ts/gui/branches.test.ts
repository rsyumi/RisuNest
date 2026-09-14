import { beforeEach, describe, expect, it, vi } from 'vitest'
import type {
    ConversationPage,
    ConversationWindow,
    PersistentRevisionLease,
} from '../storage/persistentDataStore'

const mocks = vi.hoisted(() => ({
    capturePersistentMutationToken: vi.fn(),
    getPersistentDataRuntime: vi.fn(),
    readPersistentCompleteCharacter: vi.fn(),
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    capturePersistentMutationToken: mocks.capturePersistentMutationToken,
    getPersistentDataRuntime: mocks.getPersistentDataRuntime,
    readPersistentCompleteCharacter: mocks.readPersistentCompleteCharacter,
}))

import { getChatBranches } from './branches'
import { scanPinnedChatBranches } from './branches'

async function sha256(value: unknown): Promise<string> {
    const bytes = new TextEncoder().encode(JSON.stringify(value))
    const digest = await globalThis.crypto.subtle.digest('SHA-256', bytes)
    return [...new Uint8Array(digest)]
        .map((byte) => byte.toString(16).padStart(2, '0'))
        .join('')
}

describe('chat branch visualization', () => {
    beforeEach(() => {
        vi.clearAllMocks()
    })

    it('pages every authoritative conversation through one pinned revision', async () => {
        const conversations = new Map([
            ['chat-a', {
                messages: [{ role: 'user' as const, data: 'selected path', chatId: 'shared' }],
            }],
            ['chat-b', {
                messages: [
                    { role: 'user' as const, data: 'nonselected path' },
                    { role: 'char' as const, data: 'tail', chatId: 'shared' },
                ],
            }],
        ])
        const queryConversations = vi.fn(async ({ cursor }): Promise<ConversationPage> => ({
            revision: 7,
            items: cursor === undefined
                ? [{
                    id: 'chat-a',
                    characterId: 'char-a',
                    name: 'Selected',
                    configuredIndex: 0,
                    recentAt: 1,
                    messageCount: 1,
                    fmIndex: -1,
                }]
                : [{
                    id: 'chat-b',
                    characterId: 'char-a',
                    name: 'Nonselected',
                    configuredIndex: 1,
                    recentAt: 2,
                    messageCount: 2,
                    fmIndex: 0,
                }],
            nextCursor: cursor === undefined ? 'next' : undefined,
        }))
        const readConversationWindow = vi.fn(async ({
            characterId,
            conversationId,
            startIndex = 0,
            limit = 128,
        }): Promise<{ revision: number; value: ConversationWindow }> => {
            const messages = conversations.get(conversationId)!.messages
            const page = messages.slice(startIndex, startIndex + limit)
            return {
                revision: 7,
                value: {
                    characterId,
                    conversationId,
                    messages: structuredClone(page),
                    startIndex,
                    endIndex: startIndex + page.length,
                    totalMessages: messages.length,
                    hasMoreBefore: startIndex > 0,
                    hasMoreAfter: startIndex + page.length < messages.length,
                },
            }
        })
        const release = vi.fn(async () => undefined)
        const lease = {
            revision: 7,
            readCharacter: vi.fn(async () => ({
                revision: 7,
                value: {
                    type: 'character',
                    chaId: 'char-a',
                    name: 'Character',
                    firstMessage: 'Greeting',
                    alternateGreetings: ['Alternate greeting'],
                    chatPage: 0,
                },
            })),
            queryConversations,
            readConversationWindow,
            readConversation: vi.fn(() => {
                throw new Error('complete conversation materialized')
            }),
            release,
        } as unknown as PersistentRevisionLease
        const store = {
            acquireRevision: vi.fn(async () => lease),
        }
        mocks.capturePersistentMutationToken.mockResolvedValue({
            revision: 7,
            mutationGeneration: 3,
        })
        mocks.getPersistentDataRuntime.mockReturnValue({ store })

        const branches = await getChatBranches('char-a')

        expect(mocks.readPersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(store.acquireRevision).toHaveBeenCalledWith(7)
        expect(queryConversations.mock.calls.map(([query]) => query)).toEqual([
            { characterId: 'char-a', order: 'configured', limit: 128 },
            { characterId: 'char-a', order: 'configured', limit: 128, cursor: 'next' },
        ])
        expect(readConversationWindow.mock.calls.every(([query]) => query.limit <= 128)).toBe(true)
        expect(lease.readConversation).not.toHaveBeenCalled()
        expect(release).toHaveBeenCalledOnce()
        expect(branches.map((branch) => branch.preview)).toContain('nonselected path')
        expect(branches.every((branch) => branch.revision === 7)).toBe(true)
        expect(branches.find((branch) => branch.preview === 'nonselected path')).toMatchObject({
            sourceConversationId: 'chat-b',
            sourceIndex: 0,
        })
    })

    it('bounds retained branch previews without changing the message identity hash', async () => {
        const data = 'x'.repeat(1_000)
        const lease = {
            revision: 8,
            readCharacter: vi.fn(async () => ({
                revision: 8,
                value: {
                    type: 'character',
                    chaId: 'char-preview',
                    name: 'Preview',
                    firstMessage: '',
                    alternateGreetings: [],
                },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 8,
                items: [{
                    id: 'chat-preview',
                    characterId: 'char-preview',
                    name: 'Preview chat',
                    configuredIndex: 0,
                    recentAt: 0,
                    messageCount: 1,
                    fmIndex: -1,
                }],
            })),
            readConversationWindow: vi.fn(async () => ({
                revision: 8,
                value: {
                    characterId: 'char-preview',
                    conversationId: 'chat-preview',
                    messages: [{ role: 'char', data }],
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: 1,
                    hasMoreBefore: false,
                    hasMoreAfter: false,
                },
            })),
            release: vi.fn(async () => undefined),
        } as unknown as PersistentRevisionLease

        const graph = await scanPinnedChatBranches(
            { acquireRevision: vi.fn(async () => lease) } as never,
            'char-preview',
            8,
        )
        const messageBranch = graph.branches.find((branch) => branch.sourceIndex === 0)!

        expect(messageBranch.preview).toHaveLength(200)
        expect(messageBranch.preview.endsWith('...')).toBe(true)
        expect(messageBranch.content).not.toBe('')
    })

    it('preserves the 10,000-turn graph hash without recursive materialization', async () => {
        const messages = Array.from({ length: 10_000 }, (_, index) => ({
            role: index % 2 === 0 ? 'user' as const : 'char' as const,
            data: `turn-${index.toString().padStart(5, '0')}`,
            ...(index % 3 === 0
                ? { chatId: 'duplicate' }
                : index % 3 === 1 ? {} : { chatId: `id-${index}` }),
        }))
        const readConversationWindow = vi.fn(async ({
            characterId,
            conversationId,
            startIndex = 0,
            limit = 128,
        }): Promise<{ revision: number; value: ConversationWindow }> => {
            const page = messages.slice(startIndex, startIndex + limit)
            return {
                revision: 11,
                value: {
                    characterId,
                    conversationId,
                    messages: page,
                    startIndex,
                    endIndex: startIndex + page.length,
                    totalMessages: messages.length,
                    hasMoreBefore: startIndex > 0,
                    hasMoreAfter: startIndex + page.length < messages.length,
                },
            }
        })
        const release = vi.fn(async () => undefined)
        const lease = {
            revision: 11,
            readCharacter: vi.fn(async () => ({
                revision: 11,
                value: {
                    type: 'character',
                    chaId: 'char-large',
                    name: 'Large',
                    firstMessage: 'Greeting',
                    alternateGreetings: [],
                },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 11,
                items: [{
                    id: 'chat-large',
                    characterId: 'char-large',
                    name: 'Large chat',
                    configuredIndex: 0,
                    recentAt: 1,
                    messageCount: messages.length,
                    fmIndex: -1,
                }],
            })),
            readConversationWindow,
            release,
        } as unknown as PersistentRevisionLease
        const store = {
            acquireRevision: vi.fn(async () => lease),
        }

        const graph = await scanPinnedChatBranches(
            store as never,
            'char-large',
            11,
        )

        expect(graph.branches).toHaveLength(10_001)
        expect(readConversationWindow).toHaveBeenCalledTimes(Math.ceil(10_000 / 128))
        expect(readConversationWindow.mock.calls.every(([query]) => query.limit <= 128)).toBe(true)
        expect(graph.branches[128]).toMatchObject({ sourceIndex: 127, revision: 11 })
        expect(graph.branches[129]).toMatchObject({ sourceIndex: 128, revision: 11 })
        expect(release).toHaveBeenCalledOnce()
        await expect(sha256(graph.branches.map((branch) => ({
            x: branch.x,
            y: branch.y,
            connectX: branch.connectX,
            connectY: branch.connectY,
            content: branch.content,
            preview: branch.preview,
            multiChild: branch.multiChild,
            chatId: branch.chatId,
            sourceConversationId: branch.sourceConversationId,
            sourceIndex: branch.sourceIndex,
            revision: branch.revision,
        })))).resolves.toBe('f8aef8c687a261c80e1ae813f739113fa14d198cc6e24156c54d8c867190450b')
    })

    it('releases the graph lease on read failure and cancellation', async () => {
        const makeLease = (
            queryConversations: PersistentRevisionLease['queryConversations'],
            release: () => Promise<void>,
        ) => ({
            revision: 9,
            readCharacter: vi.fn(async () => ({
                revision: 9,
                value: {
                    type: 'character',
                    chaId: 'char-a',
                    name: 'Character',
                    firstMessage: 'Greeting',
                },
            })),
            queryConversations,
            release,
        }) as unknown as PersistentRevisionLease

        const failedRelease = vi.fn(async () => undefined)
        const failedLease = makeLease(
            vi.fn(async () => {
                throw new Error('graph read failed')
            }),
            failedRelease,
        )
        await expect(scanPinnedChatBranches(
            { acquireRevision: vi.fn(async () => failedLease) } as never,
            'char-a',
            9,
        )).rejects.toThrow('graph read failed')
        expect(failedRelease).toHaveBeenCalledOnce()

        const controller = new AbortController()
        const cancelledRelease = vi.fn(async () => undefined)
        const cancelledLease = makeLease(
            vi.fn(async () => {
                controller.abort(new Error('cancel graph scan'))
                return {
                    revision: 9,
                    items: [{
                        id: 'chat-a',
                        characterId: 'char-a',
                        name: 'Chat',
                        configuredIndex: 0,
                        recentAt: 0,
                        messageCount: 0,
                        fmIndex: -1,
                    }],
                }
            }),
            cancelledRelease,
        )
        await expect(scanPinnedChatBranches(
            { acquireRevision: vi.fn(async () => cancelledLease) } as never,
            'char-a',
            9,
            { signal: controller.signal },
        )).rejects.toThrow('cancel graph scan')
        expect(cancelledRelease).toHaveBeenCalledOnce()
    })
})
