import 'fake-indexeddb/auto'
import { expect, it, vi } from 'vitest'
import type { Chat, Database } from './storage/database.svelte'
import { IndexedDbPersistentDataStore } from './storage/indexedDbPersistentDataStore'
import { capturePersistentRoot, createPersistentDataRuntime } from './storage/persistentDataRuntime'
import { generateResponseCandidate, moveResponseCandidate, recoverInterruptedReroll } from './durableReroll'
import { captureGenerationConversationOperation } from './process/generationConversationOperation'
vi.mock('./storage/database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No live database in synthetic storage tests')
    },
    presetTemplate: {},
}))
vi.mock('./globalApi.svelte', () => ({ forageStorage: {} }))

import { decodeRisuSave } from './storage/risuSave'
import { streamRisuSaveFromStore } from './storage/risuSaveStoreAdapter'

it('keeps candidates and recovery through real session commits, IndexedDB reopen and RisuSave export', async () => {
    let serial = 0
    let db = {
        username: 'Synthetic',
        botPresets: [],
        characters: [
            {
                type: 'character',
                chaId: 'char',
                chatPage: 0,
                chats: [
                    {
                        id: 'chat',
                        name: 'Synthetic',
                        note: '',
                        localLore: [],
                        savedToggleValues: {},
                        bindedPersona: 'persona',
                        message: Array.from({ length: 10_000 }, (_, index) => ({
                            role: index % 2 ? 'char' : 'user',
                            data: `synthetic-${index}`,
                            chatId: `m-${index}`,
                        })),
                    },
                ],
            },
        ],
    } as unknown as Database
    const store = new IndexedDbPersistentDataStore(
        `pocket-controls-${crypto.randomUUID()}`,
        indexedDB,
        IDBKeyRange,
    )
    await store.open()
    await store.replaceFromDatabase(db)
    const runtime = createPersistentDataRuntime({
        store,
        prepareDatabase: async (value) => value,
        state: {
            captureRoot: () => capturePersistentRoot(db),
            captureSelectedCharacter: () => db.characters[0],
            captureCharacter: (id) => db.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => 'char',
            getSelectedConversationId: () => 'chat',
            replaceDatabase: (value) => {
                db = value
            },
            publishCharacter: (value) => {
                db.characters[0] = value
            },
            publishConversation: (_id, value) => {
                db.characters[0].chats[0] = value
            },
            canUseWindowedSelectedConversation: () => false,
            isConversationOperationActive: () => true,
        },
    })
    await runtime.initializeActiveWorkingSet(db)
    const lease = await runtime.acquireCompleteConversation('synthetic-reroll')
    const chat = db.characters[0].chats[0]
    const read = vi.spyOn(lease.session, 'readRange')
    let interrupted: Chat | undefined
    let fail = true
    const options = {
        chat,
        session: () => lease.session,
        createId: () => `candidate-${++serial}`,
        isCurrent: () => true,
        aborted: () => false,
        flush: () => runtime.flushPendingData('synthetic-reroll'),
        generate: async () => {
            const operation = captureGenerationConversationOperation({
                session: lease.session,
                getCurrentSession: () => lease.session,
                chat,
                getCurrentChat: () => chat,
                append: { role: 'char', data: '', chatId: `generation-${serial}` },
            })
            operation.commitData('Synthetic streamed response {{inlay::inactive-image}}')
            await runtime.flushPendingData('synthetic-stream')
            interrupted = (await store.readConversation('char', 'chat'))!.value
            operation.release()
            return !fail
        },
    }
    expect(await generateResponseCandidate(options)).toBe(false)
    expect((await store.readConversation('char', 'chat'))!.value.message.at(-1)?.data).toBe('synthetic-9999')
    recoverInterruptedReroll(interrupted!, null)
    expect(interrupted!.message.at(-1)?.data).toBe('synthetic-9999')
    fail = false
    expect(await generateResponseCandidate(options)).toBe(true)
    expect(chat.message.at(-1)?.chatId).toBe('m-9999')
    moveResponseCandidate(chat, lease.session, -1, options.createId)
    await runtime.flushPendingData('synthetic-select')
    expect(read.mock.calls.every(([, limit]) => limit < 10_000)).toBe(true)
    lease.release()
    const reopened = (await store.readConversation('char', 'chat'))!.value
    expect(reopened.message.at(-1)?.responseVariants?.candidates).toHaveLength(2)
    expect(reopened.savedToggleValues).toEqual({})
    expect(reopened.bindedPersona).toBe('persona')
    const chunks: Uint8Array[] = []
    for await (const chunk of streamRisuSaveFromStore(store, runtime.revision)) chunks.push(chunk)
    const bytes = new Uint8Array(chunks.reduce((sum, chunk) => sum + chunk.length, 0))
    let offset = 0
    for (const chunk of chunks) {
        bytes.set(chunk, offset)
        offset += chunk.length
    }
    const exported = (await decodeRisuSave(bytes)) as Database
    expect(exported.characters[0].chats[0].message.at(-1)?.responseVariants).toEqual(
        reopened.message.at(-1)?.responseVariants,
    )
})
