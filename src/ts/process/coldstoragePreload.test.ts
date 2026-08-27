import { beforeEach, describe, expect, it, vi } from 'vitest'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { coldStorageHeader } from './coldstorageData'

const mocks = vi.hoisted(() => ({
    activeSession: null as ActiveConversationSession | null,
    database: { characters: [] as any[] },
    navigationGeneration: 0,
    selectedIndex: 0,
    selectedCharID: {
        subscribe(run: (value: number) => void) {
            run(mocks.selectedIndex)
            return () => undefined
        },
    },
}))

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

vi.mock('../sionyw', () => ({ fetchProtectedResource: vi.fn() }))
vi.mock('../globalApi.svelte', () => ({ forageStorage: { isAccount: false } }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('../stores.svelte', () => ({
    DBState: { db: mocks.database },
    selectedCharID: mocks.selectedCharID,
}))
vi.mock('../alert', () => ({
    alertClear: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertWait: vi.fn(),
}))
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('../storage/coldStorageCompaction', () => ({ compactColdStorageDatabase: vi.fn() }))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => mocks.activeSession,
    getPersistentNavigationGeneration: () => mocks.navigationGeneration,
    replacePersistentDatabase: vi.fn(),
}))

describe('cold storage selected conversation preload', () => {
    beforeEach(() => {
        mocks.activeSession = null
        mocks.database.characters = []
        mocks.navigationGeneration = 0
        mocks.selectedIndex = 0
    })

    it('publishes a complete cold payload through the active versioned session', async () => {
        const chat = {
            id: 'chat-a',
            name: 'Chat A',
            message: [{ role: 'char', data: `${coldStorageHeader}cold-a` }],
        } as any
        const character = {
            chaId: 'character-a',
            chatPage: 0,
            chats: [chat],
        }
        mocks.database.characters = [character]
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: chat.id,
            conversation: chat,
            storeRevision: 7,
            onMutation,
        })
        mocks.activeSession = session
        const { configureLocalColdStorageRuntime, preLoadChat } = await import(
            './coldstorage.svelte'
        )
        configureLocalColdStorageRuntime({
            read: vi.fn(async () => ({
                message: [{ role: 'user', data: 'restored' }],
                hypaV2Data: { indexed: true },
                scriptstate: { $value: 'restored' },
            })),
        } as any)

        await preLoadChat(0, 0)

        expect(chat).toMatchObject({
            message: [{ role: 'user', data: 'restored' }],
            hypaV2Data: { indexed: true },
            scriptstate: { $value: 'restored' },
        })
        expect(session.version).toBe(1)
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            characterId: character.chaId,
            conversationId: chat.id,
            previousVersion: 0,
            sessionVersion: 1,
            commands: ['replace-conversation'],
        }))
    })

    it('does not mutate the previous conversation after selection and session navigation', async () => {
        const chat = {
            id: 'chat-a',
            name: 'Chat A',
            message: [{ role: 'char', data: `${coldStorageHeader}cold-a` }],
        } as any
        const character = {
            chaId: 'character-a',
            chatPage: 0,
            chats: [chat],
        }
        const nextChat = {
            id: 'chat-b',
            name: 'Chat B',
            message: [{ role: 'user', data: 'next chat' }],
        } as any
        const nextCharacter = {
            chaId: 'character-b',
            chatPage: 0,
            chats: [nextChat],
        }
        mocks.database.characters = [character, nextCharacter]
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: chat.id,
            conversation: chat,
            storeRevision: 7,
        })
        mocks.activeSession = session
        const pending = deferred<unknown>()
        const { configureLocalColdStorageRuntime, preLoadChat } = await import(
            './coldstorage.svelte'
        )
        configureLocalColdStorageRuntime({
            read: vi.fn(() => pending.promise),
        } as any)

        const preload = preLoadChat(0, 0)
        mocks.selectedIndex = 1
        mocks.activeSession = new ActiveConversationSession({
            characterId: nextCharacter.chaId,
            conversationId: nextChat.id,
            conversation: nextChat,
            storeRevision: 7,
        })
        pending.resolve([{ role: 'user', data: 'stale restored payload' }])
        await preload

        expect(chat.message).toEqual([
            { role: 'char', data: `${coldStorageHeader}cold-a` },
        ])
        expect(session.version).toBe(0)
        expect(nextChat.message).toEqual([{ role: 'user', data: 'next chat' }])
    })

    it('rejects a no-session preload after navigation changes away and back', async () => {
        const chat = {
            id: 'chat-a',
            name: 'Chat A',
            message: [{ role: 'char', data: `${coldStorageHeader}cold-a` }],
        } as any
        const character = {
            chaId: 'character-a',
            chatPage: 0,
            chats: [chat],
        }
        mocks.database.characters = [character]
        const pending = deferred<unknown>()
        const { configureLocalColdStorageRuntime, preLoadChat } = await import(
            './coldstorage.svelte'
        )
        configureLocalColdStorageRuntime({
            read: vi.fn(() => pending.promise),
        } as any)

        const preload = preLoadChat(0, 0)
        mocks.selectedIndex = 1
        mocks.navigationGeneration += 1
        mocks.selectedIndex = 0
        pending.resolve([{ role: 'user', data: 'stale restored payload' }])
        await preload

        expect(chat.message).toEqual([
            { role: 'char', data: `${coldStorageHeader}cold-a` },
        ])
    })

    it('preserves the direct compatibility path when no session or navigation change exists', async () => {
        const chat = {
            id: 'chat-a',
            name: 'Chat A',
            message: [{ role: 'char', data: `${coldStorageHeader}cold-a` }],
        } as any
        const character = {
            chaId: 'character-a',
            chatPage: 0,
            chats: [chat],
        }
        mocks.database.characters = [character]
        const { configureLocalColdStorageRuntime, preLoadChat } = await import(
            './coldstorage.svelte'
        )
        configureLocalColdStorageRuntime({
            read: vi.fn(async () => [{ role: 'user', data: 'restored fallback' }]),
        } as any)

        await preLoadChat(0, 0)

        expect(chat.message).toEqual([{ role: 'user', data: 'restored fallback' }])
    })

    it('rejects a cold payload when the captured session version advances', async () => {
        const chat = {
            id: 'chat-a',
            name: 'Chat A',
            message: [{ role: 'char', data: `${coldStorageHeader}cold-a` }],
        } as any
        const character = {
            chaId: 'character-a',
            chatPage: 0,
            chats: [chat],
        }
        mocks.database.characters = [character]
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: chat.id,
            conversation: chat,
            storeRevision: 7,
        })
        mocks.activeSession = session
        const pending = deferred<unknown>()
        const { configureLocalColdStorageRuntime, preLoadChat } = await import(
            './coldstorage.svelte'
        )
        configureLocalColdStorageRuntime({
            read: vi.fn(() => pending.promise),
        } as any)

        const preload = preLoadChat(0, 0)
        session.append({ role: 'user', data: 'new local message' } as any)
        pending.resolve([{ role: 'user', data: 'stale restored payload' }])
        await preload

        expect(chat.message).toEqual([
            { role: 'char', data: `${coldStorageHeader}cold-a` },
            { role: 'user', data: 'new local message' },
        ])
        expect(session.version).toBe(1)
    })

    it.each([true, false])(
        'rejects an in-place placeholder replacement with active session %s',
        async (withSession) => {
            const chat = {
                id: 'chat-a',
                name: 'Chat A',
                message: [{ role: 'char', data: `${coldStorageHeader}cold-a` }],
            } as any
            const character = {
                chaId: 'character-a',
                chatPage: 0,
                chats: [chat],
            }
            mocks.database.characters = [character]
            const session = withSession
                ? new ActiveConversationSession({
                    characterId: character.chaId,
                    conversationId: chat.id,
                    conversation: chat,
                    storeRevision: 7,
                })
                : null
            mocks.activeSession = session
            const pending = deferred<unknown>()
            const { configureLocalColdStorageRuntime, preLoadChat } = await import(
                './coldstorage.svelte'
            )
            configureLocalColdStorageRuntime({
                read: vi.fn(() => pending.promise),
            } as any)

            const preload = preLoadChat(0, 0)
            const replacementPlaceholder = {
                role: 'char',
                data: `${coldStorageHeader}cold-a`,
                time: 123,
            }
            chat.message[0] = replacementPlaceholder
            pending.resolve([{ role: 'user', data: 'stale restored payload' }])
            await preload

            expect(chat.message[0]).toBe(replacementPlaceholder)
            expect(chat.message).toEqual([replacementPlaceholder])
            expect(session?.version ?? 0).toBe(0)
        },
    )
})
