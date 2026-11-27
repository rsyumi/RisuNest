import { bench, describe } from 'vitest'
import { ActiveConversationSession } from './storage/activeConversationSession'
import { SynchronousSessionConversationViewportSource } from './conversationViewportSource'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { IndexedDbPersistentDataStore } from './storage/indexedDbPersistentDataStore'
import { WindowedConversationOverlay } from './storage/windowedConversationOverlay'
import { PersistentConversationViewportSource } from './conversationViewportSource'
import { fixtureDatabase } from './storage/tests/persistentDataFixtures'
import type { ConversationWindowQuery } from './storage/persistentDataStore'
import type { Chat, character, Message } from './storage/database.svelte'

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


for (const count of [10_000, 100_000]) {
    describe(`${count} stored rows`, () => {
        const store = new IndexedDbPersistentDataStore('windowed-benchmark', new IDBFactory(), IDBKeyRange)
        const characterId = 'char-a', conversationId = 'conv-long'
        let overlay: WindowedConversationOverlay
        let source: PersistentConversationViewportSource
        let published = 0
        let revision = 0
        let reads = 0
        let anchors = 0
        let windows = 0
        const counted = (input: ConversationWindowQuery) => {
            if (input.anchorMessageId !== undefined) anchors++
            else windows++
        }
        async function prepare() {
            if (source) return
            await store.open()
            const database = structuredClone(fixtureDatabase)
            database.characters.find(character => character.chaId === characterId)!.chats[0].message =
                Array.from({ length: count }, (_, i) => ({ chatId: `m${i}`, role: i % 2 ? 'char' : 'user', data: 'synthetic' }))
            revision = (await store.replaceFromDatabase(database)).revision
            overlay = new WindowedConversationOverlay(characterId, conversationId, revision, count)
            source = new PersistentConversationViewportSource({ characterId, conversationId, revision, totalMessages: count, overlay,
                rowBudget: 4, reader: { readConversationWindow: query => { counted(query); return store.readConversationWindow(query) } } })
        }
        for (const scenario of ['middle edit', 'tail edit', 'middle swap', 'middle deletion'] as const) {
            let recorded = false
            bench(scenario, async () => {
                reads = 0
                anchors = 0
                windows = 0
                const at = scenario === 'tail edit' ? overlay.count - 1 : Math.floor(overlay.count / 2)
                const length = scenario === 'middle swap' ? 2 : 1
                const current = await source.readProjectedRange(at, scenario === 'middle deletion' ? 2 : length)
                const before = current.slice(0, length)
                const messages: Message[] = scenario === 'middle deletion' ? [] : scenario === 'middle swap' ? [...before].reverse()
                    : [{ ...before[0], data: `published-${++published}` }]
                const epoch = overlay.epoch
                overlay.applyRange(at, before, messages, ++published)
                source.refreshProjection({start: at, end: at + messages.length, structureChanged: overlay.epoch !== epoch})
                const prefix = overlay.freeze()
                const lease = await store.acquireRevision(revision)
                reads++
                const plan = await overlay.plan(prefix, { revision, readConversationWindow: query => {
                    counted(query)
                    return lease.readConversationWindow(query)
                } }, overlay.storedCount).finally(() => lease.release())
                const bodies = overlay.intervals(0, overlay.count).reduce((n, range) => n + ('messages' in range ? range.length : 0), 0)
                const range = plan.mutations[0]
                if (plan.mutations.length !== 1 || range.start !== at || range.deleteCount !== length || range.messages.length !== messages.length)
                    throw new Error('The save touched a different range')
                const t = performance.now()
                revision = (await store.commit({expectedRevision: revision, conversations: plan.mutations})).revision
                const commitMs = performance.now() - t
                overlay.acknowledge(plan, revision)
                source.installProjection(revision, overlay.count, false)
                if (!recorded) console.log(JSON.stringify({count, scenario, reads, anchors, windows, bodies,
                    resident: source.captureResidentRows().length, suffix: scenario === 'middle deletion' ? overlay.count - at : 0,
                    ranges: plan.mutations.map(m => [m.start, m.deleteCount, m.messages.length]), commitMs}))
                const durable = await store.readConversationWindow({ characterId, conversationId, startIndex: at, limit: Math.max(1, messages.length) })
                const expected = messages.length ? messages : current.slice(1)
                if (!durable || durable.value.totalMessages !== overlay.count || JSON.stringify(durable.value.messages) !== JSON.stringify(expected))
                    throw new Error('The saved edit or its shifted neighbour was lost')
                recorded = true
            }, { setup: prepare, time: 1, iterations: 1, warmupIterations: 0, warmupTime: 0 })
        }
    })
}
