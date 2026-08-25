import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import type {
    CharacterSummary,
    ConversationSummary,
    PersistentRevisionReader,
} from '../persistentDataStore'
import {
    formatNameList,
    summarizePinnedSyncConflict,
    summarizeSyncConflict,
} from './syncConflictSummary'

interface CharacterSeed {
    chaId: string
    name?: string
    chats?: Array<{ message: unknown[] }>
}

function makeDatabase(characters: CharacterSeed[]): Database {
    return { characters } as unknown as Database
}

describe('summarizeSyncConflict', () => {
    it('splits characters into local-only, remote-only, and changed groups', () => {
        const local = makeDatabase([
            { chaId: 'a', name: 'Alice', chats: [{ message: [1] }] },
            { chaId: 'b', name: 'Bell', chats: [{ message: [] }] },
            { chaId: 'c', name: 'Cory', chats: [{ message: [1, 2] }] },
        ])
        const remote = makeDatabase([
            { chaId: 'a', name: 'Alice', chats: [{ message: [1] }] },
            { chaId: 'c', name: 'Cory', chats: [{ message: [1, 2, 3] }] },
            { chaId: 'd', name: 'Dana', chats: [] },
        ])

        expect(summarizeSyncConflict(local, remote)).toEqual({
            localOnlyNames: ['Bell'],
            remoteOnlyNames: ['Dana'],
            changedNames: ['Cory'],
        })
    })

    it('marks a character as changed when its name or chat count differs', () => {
        const local = makeDatabase([
            { chaId: 'a', name: 'Old name', chats: [{ message: [] }] },
            { chaId: 'b', name: 'Same', chats: [{ message: [] }] },
        ])
        const remote = makeDatabase([
            { chaId: 'a', name: 'New name', chats: [{ message: [] }] },
            { chaId: 'b', name: 'Same', chats: [{ message: [] }, { message: [] }] },
        ])

        expect(summarizeSyncConflict(local, remote).changedNames).toEqual(['Old name', 'Same'])
    })

    it('falls back to the character id when the name is empty and tolerates missing arrays', () => {
        const local = makeDatabase([{ chaId: 'no-name', name: '  ' }])
        const remote = makeDatabase([])

        expect(summarizeSyncConflict(local, remote)).toEqual({
            localOnlyNames: ['no-name'],
            remoteOnlyNames: [],
            changedNames: [],
        })
    })

    it('reads late local descriptors from bounded pages without loading record bodies', async () => {
        const character = (
            id: string,
            name: string,
            configuredIndex: number,
            trashed = false,
        ): CharacterSummary => ({
            id,
            name,
            image: '',
            configuredIndex,
            recentAt: 0,
            trashed,
            conversationCount: id === 'shared' ? 129 : 0,
            type: 'character',
        })
        const active = [
            character('shared', 'Shared', 0),
            ...Array.from({ length: 127 }, (_, index) => (
                character(`matched-${index}`, `Matched ${index}`, index + 1)
            )),
            character('late-local', 'Late local', 200),
        ]
        const trash = [character('trash-local', 'Trash local', 64.5, true)]
        const conversations = Array.from({ length: 129 }, (_, index): ConversationSummary => ({
            id: `chat-${index}`,
            characterId: 'shared',
            name: `Chat ${index}`,
            configuredIndex: index,
            recentAt: index,
            messageCount: index === 128 ? 2 : 1,
        }))
        const readCharacter = vi.fn()
        const readConversation = vi.fn()
        const reader = {
            revision: 8,
            queryCharacters: vi.fn(async ({ trash: trashed, cursor }: {
                trash: boolean
                cursor?: string
            }) => {
                const values = trashed ? trash : active
                const start = cursor ? Number(cursor) : 0
                const end = Math.min(start + 128, values.length)
                return {
                    revision: 8,
                    items: values.slice(start, end),
                    nextCursor: end < values.length ? String(end) : undefined,
                }
            }),
            queryConversations: vi.fn(async ({ characterId, cursor }: {
                characterId: string
                cursor?: string
            }) => {
                const values = characterId === 'shared' ? conversations : []
                const start = cursor ? Number(cursor) : 0
                const end = Math.min(start + 128, values.length)
                return {
                    revision: 8,
                    items: values.slice(start, end),
                    nextCursor: end < values.length ? String(end) : undefined,
                }
            }),
            readCharacter,
            readConversation,
        } as unknown as PersistentRevisionReader
        const remote = makeDatabase([
            { chaId: 'shared', name: 'Shared', chats: Array.from({ length: 129 }, (_, index) => ({
                message: index === 128 ? [1] : [1],
            })) },
            ...active.slice(1, -1).map((summary) => ({
                chaId: summary.id,
                name: summary.name,
                chats: [],
            })),
            { chaId: 'remote-only', name: 'Remote only', chats: [] },
        ])

        await expect(summarizePinnedSyncConflict(reader, remote)).resolves.toEqual({
            localOnlyNames: ['Trash local', 'Late local'],
            remoteOnlyNames: ['Remote only'],
            changedNames: ['Shared'],
        })
        expect(readCharacter).not.toHaveBeenCalled()
        expect(readConversation).not.toHaveBeenCalled()
    })
})

describe('formatNameList', () => {
    it('joins short lists directly', () => {
        expect(formatNameList(['A', 'B'])).toBe('A, B')
    })

    it('truncates long lists with a remainder count', () => {
        expect(formatNameList(['A', 'B', 'C', 'D', 'E'])).toBe('A, B, C +2')
    })

    it('returns an empty string for an empty list', () => {
        expect(formatNameList([])).toBe('')
    })
})
