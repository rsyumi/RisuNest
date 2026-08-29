import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import {
    queryChatMessageTargetAt,
    queryChatMessageTargetById,
    queryChatMessageTargetsByIds,
    renameCapturedBookmark,
} from '../chatMessageUi'
import type { Chat, Database, Message } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { RevisionConflictError, type WorkingSetCommit } from './persistentDataStore'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'

const INITIAL_MESSAGE_COUNT = 10_000
const VIEWPORT_ROW_BUDGET = 64

function makeMessage(index: number): Message {
    return {
        role: index % 2 === 0 ? 'user' : 'char',
        data: `turn-${index.toString().padStart(5, '0')}`,
        chatId: `message-${index}`,
        name: index % 97 === 0 ? `speaker-${index}` : undefined,
        saying: index % 131 === 0 ? `aside-${index}` : undefined,
    }
}

function makeConversation(): Chat {
    return {
        id: 'chat-a',
        name: 'Corpus conversation',
        note: 'synthetic 10,000-turn owner',
        localLore: [],
        fmIndex: -1,
        message: Array.from({ length: INITIAL_MESSAGE_COUNT }, (_, index) => makeMessage(index)),
    }
}

function makeDatabase(conversation: Chat): Database {
    return {
        username: 'Eviction corpus',
        botPresets: [],
        pluginCustomStorage: {},
        characters: [{
            type: 'character',
            chaId: 'char-a',
            name: 'Synthetic owner',
            firstMessage: 'Greeting',
            alternateGreetings: [],
            chatPage: 0,
            chats: [conversation],
        }],
    } as unknown as Database
}

async function waitForWindowed(runtime: ReturnType<typeof createPersistentDataRuntime>) {
    await vi.waitFor(() => expect(runtime.getSelectedConversationMode()).toBe('windowed'))
    expect(runtime.getActiveConversationSession()).toBeNull()
    const source = runtime.getActiveConversationViewportSource()
    expect(source).not.toBeNull()
    expect(source!.snapshot().totalMessages).toBeGreaterThan(0)
}

describe('selected conversation eviction correctness corpus', () => {
    it('matches a complete-owner oracle across forced 10,000-turn demotion cycles', async () => {
        const initialConversation = makeConversation()
        const oracle = structuredClone(initialConversation)
        let workingCopy = structuredClone(makeDatabase(initialConversation))
        const store = new IndexedDbPersistentDataStore(
            `selected-eviction-corpus-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const initial = await store.replaceFromDatabase(workingCopy)
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((candidate) => candidate.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () => workingCopy.characters[0]?.chats[0]?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, conversation, nextCharacter) => {
                if (nextCharacter) workingCopy.characters[0] = nextCharacter
                else workingCopy.characters[0].chats[0] = conversation
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
            conversationViewportRowBudget: VIEWPORT_ROW_BUDGET,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)
        await waitForWindowed(runtime)
        let expectedRevision = initial.revision

        const context = {
            captureCurrent: () => ({
                character: workingCopy.characters[0],
                conversation: workingCopy.characters[0].chats[0],
            }),
            getCurrentSession: () => runtime.getActiveConversationSession(),
            captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
            acquirePersistentRevision: (revision: number) => store.acquireRevision(revision),
            acquireCompleteConversation: (
                reason: string,
                target = runtime.captureSelectedConversationTarget(),
            ) => runtime.acquireCompleteConversation(reason, target),
        }

        const assertWindowed = async () => {
            await waitForWindowed(runtime)
            expect(() => workingCopy.characters[0].chats[0].message).toThrow('metadata-only')
            expect(runtime.captureSelectedConversationAuthority()).toMatchObject({
                characterId: 'char-a',
                conversationId: 'chat-a',
                storeRevision: expectedRevision,
                totalMessages: oracle.message.length,
            })
        }

        const mutateComplete = async (
            reason: string,
            mutateRuntime: (session: NonNullable<ReturnType<typeof runtime.getActiveConversationSession>>) => void,
            mutateOracle: () => void,
            failure?: Error,
        ) => {
            const target = runtime.captureSelectedConversationTarget()
            const lease = await runtime.acquireCompleteConversation(reason, target)
            expect(lease.session.totalMessages).toBe(oracle.message.length)
            expect('evictionEnabled' in lease.session).toBe(false)
            mutateRuntime(lease.session)
            mutateOracle()
            lease.release()

            const originalCommit = store.commit.bind(store)
            const commitSpy = failure
                ? vi.spyOn(store, 'commit').mockRejectedValueOnce(failure)
                : null
            if (failure) {
                await expect(runtime.flushPendingData(`${reason}-failed`)).rejects.toBe(failure)
                expect(runtime.getSelectedConversationMode()).toBe('complete')
                commitSpy!.mockImplementation((commit: WorkingSetCommit) => originalCommit(commit))
            }
            await runtime.flushPendingData(`${reason}-persisted`)
            expectedRevision += 1
            commitSpy?.mockRestore()
            await assertWindowed()
        }

        await mutateComplete(
            'append',
            (session) => session.append({ role: 'user', data: 'appended', chatId: 'op-append' }),
            () => { oracle.message.push({ role: 'user', data: 'appended', chatId: 'op-append' }) },
        )
        await mutateComplete(
            'edit',
            (session) => session.edit(session.locate(8_888), {
                ...session.readMessage(session.locate(8_888)),
                data: 'edited far turn',
                saying: 'edit metadata',
            }),
            () => { oracle.message[8_888] = { ...oracle.message[8_888], data: 'edited far turn', saying: 'edit metadata' } },
        )
        await mutateComplete(
            'delete',
            (session) => session.delete(session.locate(session.totalMessages - 3)),
            () => { oracle.message.splice(oracle.message.length - 3, 1) },
        )
        await mutateComplete(
            'truncate',
            (session) => session.truncate(session.locate(session.totalMessages - 2)),
            () => { oracle.message.splice(oracle.message.length - 2) },
        )
        await mutateComplete(
            'reroll',
            (session) => session.reroll(session.positionAt(session.totalMessages - 1), [{
                role: 'char',
                data: 'rerolled tail',
                chatId: 'op-reroll',
                generationInfo: { model: 'synthetic' },
            }]),
            () => oracle.message.splice(oracle.message.length - 1, 1, {
                role: 'char',
                data: 'rerolled tail',
                chatId: 'op-reroll',
                generationInfo: { model: 'synthetic' },
            }),
        )

        const bookmarkTarget = await queryChatMessageTargetById(context, 'message-9000')
        expect(bookmarkTarget?.kind).toBe('persistent')
        const bookmarkLease = await runtime.acquireCompleteConversation(
            'bookmark',
            bookmarkTarget!.kind === 'persistent' ? bookmarkTarget!.selection : null,
        )
        bookmarkLease.session.setBookmark(bookmarkLease.session.locate(bookmarkTarget!.absoluteIndex), {
            bookmarked: true,
            name: 'Far bookmark',
        })
        bookmarkLease.release()
        oracle.bookmarks = ['message-9000']
        oracle.bookmarkNames = { 'message-9000': 'Far bookmark' }
        await runtime.flushPendingData('bookmark-persisted')
        expectedRevision += 1
        await assertWindowed()
        const renameTarget = await queryChatMessageTargetById(context, 'message-9000')
        await expect(renameCapturedBookmark(renameTarget!, context, async () => 'Renamed bookmark'))
            .resolves.toBe(true)
        oracle.bookmarkNames['message-9000'] = 'Renamed bookmark'
        await runtime.flushPendingData('bookmark-rename-persisted')
        expectedRevision += 1
        await assertWindowed()

        await mutateComplete(
            'failed-save-retry',
            (session) => session.append({ role: 'user', data: 'retry retained', chatId: 'op-retry' }),
            () => { oracle.message.push({ role: 'user', data: 'retry retained', chatId: 'op-retry' }) },
            new Error('synthetic save failure'),
        )
        await mutateComplete(
            'revision-conflict',
            (session) => session.append({ role: 'user', data: 'conflict retained', chatId: 'op-conflict' }),
            () => { oracle.message.push({ role: 'user', data: 'conflict retained', chatId: 'op-conflict' }) },
            new RevisionConflictError(expectedRevision, expectedRevision + 1),
        )

        for (const [consumer, id] of [
            ['Trigger', 'op-trigger'],
            ['Lua', 'op-lua'],
            ['CBS', 'op-cbs'],
            ['regex', 'op-regex'],
            ['generation', 'op-generation'],
        ] as const) {
            await mutateComplete(
                consumer,
                (session) => session.append({
                    role: consumer === 'generation' ? 'char' : 'user',
                    data: `${consumer} compatibility output`,
                    chatId: id,
                    ...(consumer === 'generation'
                        ? { generationInfo: { model: 'synthetic-generator' } }
                        : { saying: `${consumer} visited complete history` }),
                }),
                () => { oracle.message.push({
                    role: consumer === 'generation' ? 'char' : 'user',
                    data: `${consumer} compatibility output`,
                    chatId: id,
                    ...(consumer === 'generation'
                        ? { generationInfo: { model: 'synthetic-generator' } }
                        : { saying: `${consumer} visited complete history` }),
                }) },
            )
        }

        const screenshot = await queryChatMessageTargetAt(context, 9_500)
        expect(screenshot).toMatchObject({
            kind: 'persistent',
            absoluteIndex: 9_500,
            message: oracle.message[9_500],
        })
        await assertWindowed()

        const search = await queryChatMessageTargetsByIds(context, [
            // Keep a mix of near, far, and generated IDs.
            'message-3',
            'message-9000',
            'op-generation',
        ])
        expect(search.map((target) => target.message)).toEqual([
            oracle.message.find((message) => message.chatId === 'message-3'),
            oracle.message.find((message) => message.chatId === 'message-9000'),
            oracle.message.find((message) => message.chatId === 'op-generation'),
        ])
        await assertWindowed()

        const hypa = await queryChatMessageTargetById(context, 'message-8888')
        expect(hypa?.message).toEqual(oracle.message.find((message) => message.chatId === 'message-8888'))
        await assertWindowed()

        const branchEnd = 257
        const branchLease = await runtime.acquireCompleteConversation('branch')
        const branchSource = branchLease.session.readBranchSource(
            branchLease.session.locate(branchEnd),
        )
        expect(branchSource).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
            startIndex: 0,
            endIndex: branchEnd + 1,
            totalMessages: oracle.message.length,
            messages: oracle.message.slice(0, branchEnd + 1),
        })
        branchLease.release()
        await assertWindowed()

        const exported = await runtime.materializePersistentDatabaseSnapshotWithRevision('export')
        expect(exported.revision).toBe(expectedRevision)
        const exportedOwner = exported.database.characters[0].chats.find((chat) => chat.id === 'chat-a')!
        expect(exportedOwner).toEqual(oracle)
        await assertWindowed()

        await mutateComplete(
            'plugin-v2.1-compatibility',
            (session) => {
                expect(session.materializeCompatibilityArray()).toEqual(oracle.message)
                session.append({
                    role: 'user',
                    data: 'plugin compatibility append',
                    chatId: 'op-plugin-v2.1',
                    saying: 'live proxy compatibility',
                })
            },
            () => { oracle.message.push({
                role: 'user',
                data: 'plugin compatibility append',
                chatId: 'op-plugin-v2.1',
                saying: 'live proxy compatibility',
            }) },
        )

        const finalPersisted = await store.readConversation('char-a', 'chat-a')
        expect(finalPersisted).not.toBeNull()
        expect(finalPersisted!.revision).toBe(expectedRevision)
        expect(finalPersisted!.value).toEqual(oracle)
        expect(finalPersisted!.value.message.map((message) => message.chatId))
            .toEqual(oracle.message.map((message) => message.chatId))
        const finalSource = runtime.getActiveConversationViewportSource()!
        await finalSource.ensureRange({
            startIndex: 9_000,
            limit: 256,
            reason: 'viewport',
        })
        const snapshot = finalSource.snapshot()
        let residentRows = 0
        for (let index = 0; index < snapshot.totalMessages; index++) {
            if (snapshot.rowAt(index) !== undefined) residentRows += 1
        }
        expect(residentRows).toBeLessThanOrEqual(VIEWPORT_ROW_BUDGET)
        expect(runtime.getActiveConversationSession()).toBeNull()
    }, 60_000)
})
