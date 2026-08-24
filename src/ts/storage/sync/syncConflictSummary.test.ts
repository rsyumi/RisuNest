import { describe, expect, it } from 'vitest'
import type { Database } from '../database.svelte'
import { formatNameList, summarizeSyncConflict } from './syncConflictSummary'

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
