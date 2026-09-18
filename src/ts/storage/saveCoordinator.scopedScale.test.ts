import { describe, expect, it, vi } from 'vitest'
import type { Chat, Database, character } from './database.svelte'
import type {
    PersistentDataStore,
    WorkingSetCommit,
} from './persistentDataStore'
import type { PersistentConversationReplacementResult } from './saveCoordinator'
import {
    captureRoot,
    SaveCoordinator,
} from './saveCoordinator.testSupport'

const CONVERSATIONS_PER_CHARACTER = 10
const MESSAGES_PER_CONVERSATION = 20
const WARM_RUNS = 5
const MEASURED_RUNS = 5

type ScaleMeasurement = {
    characterCount: number
    durationsMs: number[]
    heapBeforeBytes: number
    heapAfterBytes: number
    peakObservedHeapUsedBytes: number
    commitCount: number
    targetConversationReads: number
    unrelatedCharacterClonePropertyReads: number
    unrelatedConversationClonePropertyReads: number
}

function makeConversation(characterIndex: number, conversationIndex: number): Chat {
    return {
        id: `chat-${characterIndex}-${conversationIndex}`,
        name: `Chat ${characterIndex}-${conversationIndex}`,
        message: Array.from({ length: MESSAGES_PER_CONVERSATION }, (_, messageIndex) => ({
            role: messageIndex % 2 === 0 ? 'user' : 'char',
            data: `Synthetic message ${characterIndex}-${conversationIndex}-${messageIndex}`,
        })),
    } as Chat
}

function makeSyntheticLibrary(characterCount: number): {
    database: Database
    unrelatedCharacterClonePropertyReads: () => number
    unrelatedConversationClonePropertyReads: () => number
    resetUnrelatedClonePropertyReads: () => void
} {
    let unrelatedCharacterReads = 0
    let unrelatedConversationReads = 0
    const targetCharacterIndex = characterCount - 1
    const characters = Array.from({ length: characterCount }, (_, characterIndex) => {
        const chats = Array.from(
            { length: CONVERSATIONS_PER_CHARACTER },
            (_, conversationIndex) => makeConversation(characterIndex, conversationIndex),
        )
        const item = {
            type: 'character',
            chaId: `char-${characterIndex}`,
            name: `Character ${characterIndex}`,
            chats,
        } as character
        if (characterIndex !== targetCharacterIndex) {
            Object.defineProperty(item, 'syntheticCloneTrap', {
                enumerable: true,
                configurable: true,
                get() {
                    unrelatedCharacterReads++
                    return characterIndex
                },
            })
            for (const [conversationIndex, conversation] of chats.entries()) {
                Object.defineProperty(conversation, 'syntheticCloneTrap', {
                    enumerable: true,
                    configurable: true,
                    get() {
                        unrelatedConversationReads++
                        return conversationIndex
                    },
                })
            }
        }
        return item
    })
    return {
        database: {
            username: 'Scoped scale fixture',
            botPresets: [],
            characters,
        } as unknown as Database,
        unrelatedCharacterClonePropertyReads: () => unrelatedCharacterReads,
        unrelatedConversationClonePropertyReads: () => unrelatedConversationReads,
        resetUnrelatedClonePropertyReads: () => {
            unrelatedCharacterReads = 0
            unrelatedConversationReads = 0
        },
    }
}

async function measureScopedConversationWrites(characterCount: number): Promise<ScaleMeasurement> {
    const fixture = makeSyntheticLibrary(characterCount)
    const { database } = fixture
    const targetCharacter = database.characters[characterCount - 1]
    const targetCharacterId = targetCharacter.chaId
    const targetConversationId = targetCharacter.chats[0].id
    let revision = 1
    let durableConversation = structuredClone(targetCharacter.chats[0])
    let peakObservedHeapUsedBytes = 0
    const sampleHeap = () => {
        peakObservedHeapUsedBytes = Math.max(
            peakObservedHeapUsedBytes,
            process.memoryUsage().heapUsed,
        )
    }
    const targetReads: Array<[string, string]> = []
    const commit = vi.fn(async (input: WorkingSetCommit) => {
        sampleHeap()
        expect(input.conversations).toHaveLength(1)
        const replacement = input.conversations![0]
        expect(replacement).toMatchObject({
            type: 'replace-range',
            characterId: targetCharacterId,
            conversationId: targetConversationId,
        })
        if (replacement.type !== 'replace-range') throw new Error('Expected conversation replacement')
        durableConversation = {
            ...structuredClone(replacement.conversation),
            message: structuredClone(replacement.messages),
        } as Chat
        return { revision: ++revision }
    })
    const store = {
        commit,
        readConversation: vi.fn(async (characterId: string, conversationId: string) => {
            sampleHeap()
            targetReads.push([characterId, conversationId])
            if (characterId !== targetCharacterId || conversationId !== targetConversationId) {
                throw new Error(`Unexpected unrelated conversation read: ${characterId}/${conversationId}`)
            }
            return { revision, value: structuredClone(durableConversation) }
        }),
        readCharacter: vi.fn(async () => {
            throw new Error('Scoped conversation write must not hydrate a character')
        }),
        queryConversations: vi.fn(async () => {
            throw new Error('Scoped conversation write must not scan conversations')
        }),
        materializeDatabase: vi.fn(async () => {
            throw new Error('Scoped conversation write must not materialize the database')
        }),
    } as unknown as PersistentDataStore
    const coordinator = new SaveCoordinator({
        store,
        captureRoot: () => captureRoot(database),
        captureSelectedCharacter: () => targetCharacter,
        captureCharacter: (id) =>
            database.characters.find((item) => item.chaId === id) ?? null,
        replaceDatabase: () => undefined,
        publishConversationReplacement: (result: PersistentConversationReplacementResult) => {
            targetCharacter.chats[0] = structuredClone(result.conversation)
        },
    })
    coordinator.initialize(revision, database)
    fixture.resetUnrelatedClonePropertyReads()

    const run = async (sample: number) => {
        const replacement = {
            ...structuredClone(targetCharacter.chats[0]),
            name: `Scoped edit ${sample}`,
        } as Chat
        const startedAt = performance.now()
        await expect(coordinator.replacePersistentConversation(
            targetCharacterId,
            targetConversationId,
            'scoped-scale-measurement',
            replacement,
        )).resolves.toBe(true)
        return performance.now() - startedAt
    }

    for (let sample = 0; sample < WARM_RUNS; sample++) await run(sample)
    commit.mockClear()
    targetReads.length = 0
    fixture.resetUnrelatedClonePropertyReads()

    const heapBeforeBytes = process.memoryUsage().heapUsed
    peakObservedHeapUsedBytes = heapBeforeBytes
    const durationsMs: number[] = []
    for (let sample = 0; sample < MEASURED_RUNS; sample++) {
        durationsMs.push(await run(WARM_RUNS + sample))
        sampleHeap()
    }
    const heapAfterBytes = process.memoryUsage().heapUsed
    peakObservedHeapUsedBytes = Math.max(peakObservedHeapUsedBytes, heapAfterBytes)

    expect(commit).toHaveBeenCalledTimes(MEASURED_RUNS)
    expect(targetReads).toEqual(Array.from(
        { length: MEASURED_RUNS },
        () => [targetCharacterId, targetConversationId],
    ))
    expect(store.readCharacter).not.toHaveBeenCalled()
    expect(store.queryConversations).not.toHaveBeenCalled()
    expect(store.materializeDatabase).not.toHaveBeenCalled()
    expect(fixture.unrelatedCharacterClonePropertyReads()).toBe(0)
    expect(fixture.unrelatedConversationClonePropertyReads()).toBe(0)

    return {
        characterCount,
        durationsMs,
        heapBeforeBytes,
        heapAfterBytes,
        peakObservedHeapUsedBytes,
        commitCount: commit.mock.calls.length,
        targetConversationReads: targetReads.length,
        unrelatedCharacterClonePropertyReads: fixture.unrelatedCharacterClonePropertyReads(),
        unrelatedConversationClonePropertyReads: fixture.unrelatedConversationClonePropertyReads(),
    }
}

describe('SaveCoordinator scoped conversation scale', () => {
    it('keeps a 100 to 1000 character edit bounded to one target', async () => {
        const measurements = []
        for (const characterCount of [100, 1_000]) {
            measurements.push(await measureScopedConversationWrites(characterCount))
        }

        console.info('scoped-conversation-scale', JSON.stringify({
            conversationsPerCharacter: CONVERSATIONS_PER_CHARACTER,
            messagesPerConversation: MESSAGES_PER_CONVERSATION,
            warmRuns: WARM_RUNS,
            measuredRuns: MEASURED_RUNS,
            measurements,
        }))
    })
})
