import { bench, describe } from 'vitest'
import { ActiveConversationSession } from './storage/activeConversationSession'
import { SynchronousSessionConversationViewportSource } from './conversationViewportSource'
import type { Chat, character } from './storage/database.svelte'

// Isolate the history-size cost of publishing streaming text. This deliberately
// excludes Markdown, plugins, persistence, layout, and WebView paint time.
for (const count of [10_000, 100_000]) {
    describe(`${count} synthetic history rows`, () => {
        const conversation = {
            id: 'stream-benchmark',
            note: '',
            name: '',
            localLore: [],
            message: Array.from({ length: count }, (_, index) => ({
                role: 'char' as const,
                data: 'synthetic',
                chatId: `row-${index}`,
            })),
        } as Chat
        const owner = {
            chaId: 'synthetic-owner',
            type: 'character',
            chatPage: 0,
            chats: [conversation],
        } as character
        const session = new ActiveConversationSession({
            characterId: owner.chaId,
            conversationId: conversation.id!,
            conversation,
            storeRevision: 1,
        })
        const source = new SynchronousSessionConversationViewportSource({
            session,
            captureCurrent: () => ({ character: owner, conversation }),
        })
        const initial = source.snapshot()
        const key = initial.keyAt(count - 1)!
        let sequence = 0
        bench(
            '25 streaming publications with stable row lookup',
            () => {
                for (let chunk = 0; chunk < 25; chunk++) {
                    session.edit(session.locate(count - 1), {
                        role: 'char',
                        data: `synthetic snapshot ${++sequence}`,
                        chatId: `row-${count - 1}`,
                    })
                    const snapshot = source.snapshot()
                    if (
                        snapshot.keyAt(count - 1) !== key ||
                        snapshot.indexOfKey(key) !== count - 1
                    ) {
                        throw new Error('Streaming changed row identity')
                    }
                }
                if (initial.indexOfKey(key) !== count - 1)
                    throw new Error('Old snapshot was mutated')
            },
            { time: 500, iterations: 3, warmupIterations: 1 },
        )
    })
}
