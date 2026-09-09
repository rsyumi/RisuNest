// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import type { character } from 'src/ts/storage/database.svelte'

vi.mock('./Chat.svelte', async () => ({
    default: (await import('./ChatMountProbe.test.svelte')).default,
}))
vi.mock('./CreatorQuote.svelte', async () => ({
    default: (await import('./ChatMountProbe.test.svelte')).default,
}))

import ChatConversationStart from './ChatConversationStart.svelte'
import { chatMountProbe, resetChatMountProbe } from './chatMountProbe'

function metadataOnlyCharacter(): character {
    const conversation = { id: 'chat-id', fmIndex: -1 } as character['chats'][number]
    Object.defineProperty(conversation, 'message', {
        get() {
            throw new Error('metadata-only conversation body was accessed')
        },
    })
    return {
        type: 'character',
        name: 'Character',
        chaId: 'character-id',
        chatPage: 0,
        chats: [conversation],
        firstMessage: 'Greeting',
        alternateGreetings: [],
        creatorNotes: '',
        removedQuotes: false,
        customscript: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: [],
    } as unknown as character
}

describe('ChatConversationStart', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        resetChatMountProbe()
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        document.body.replaceChildren()
    })

    test('uses the supplied total count for the empty warning without reading the shell body', () => {
        mounted = mount(ChatConversationStart, {
            target,
            props: {
                currentCharacter: metadataOnlyCharacter(),
                resolvedImage: '',
                showAiWarning: true,
                totalMessages: 0,
                onReroll: () => {},
                unReroll: () => {},
                onRemoveCreatorQuote: () => {},
            },
        })

        expect(target.querySelector('.italic')).not.toBeNull()
    })

    test('keeps the greeting mounted while a reload prepares its replacement parser lease', async () => {
        const release = vi.fn()
        let finishReload!: (value: null) => void
        const acquire = vi
            .fn()
            .mockResolvedValueOnce({ release })
            .mockImplementationOnce(
                () =>
                    new Promise((resolve) => {
                        finishReload = resolve
                    }),
            )
        mounted = mount(ChatConversationStart, {
            target,
            props: {
                currentCharacter: metadataOnlyCharacter(),
                resolvedImage: '',
                showAiWarning: false,
                totalMessages: 0,
                onReroll: () => {},
                unReroll: () => {},
                onRemoveCreatorQuote: () => {},
                acquireConversationStartParserLease: acquire,
            },
        })
        await vi.waitFor(() => expect(chatMountProbe.mounts).toHaveLength(1))
        const greeting = target.querySelector('[data-chat-probe]')
        ;(
            mounted as { refreshConversationStartParser(): void }
        ).refreshConversationStartParser()
        await vi.waitFor(() => expect(acquire).toHaveBeenCalledTimes(2))
        expect(release).toHaveBeenCalledOnce()
        expect(chatMountProbe.mounts[0].parserAbortSignal?.aborted).toBe(true)
        expect(target.querySelector('[data-chat-probe]')).toBe(greeting)
        expect(chatMountProbe.unmounts).toHaveLength(0)
        finishReload(null)
        await Promise.resolve()
        expect(target.querySelector('[data-chat-probe]')).toBe(greeting)
        expect(chatMountProbe.mounts).toHaveLength(1)
    })

    test('aborts the greeting Chat signal before releasing its complete lease', async () => {
        let acquiredSignal: AbortSignal | undefined
        const abortedAtRelease: boolean[] = []
        const release = vi.fn(() =>
            abortedAtRelease.push(acquiredSignal?.aborted ?? false),
        )
        mounted = mount(ChatConversationStart, {
            target,
            props: {
                currentCharacter: metadataOnlyCharacter(),
                resolvedImage: '',
                showAiWarning: false,
                totalMessages: 0,
                onReroll: () => {},
                unReroll: () => {},
                onRemoveCreatorQuote: () => {},
                acquireConversationStartParserLease: async ({ signal }) => {
                    acquiredSignal = signal
                    return { release }
                },
            },
        })
        await vi.waitFor(() => expect(chatMountProbe.mounts).toHaveLength(1))
        const chatSignal = chatMountProbe.mounts[0].parserAbortSignal
        expect(chatSignal).toBeDefined()
        expect(chatSignal).toBe(acquiredSignal)
        expect(chatSignal?.aborted).toBe(false)
        await unmount(mounted)
        mounted = undefined
        expect(chatSignal?.aborted).toBe(true)
        expect(release).toHaveBeenCalledOnce()
        expect(abortedAtRelease).toEqual([true])
    })

})
