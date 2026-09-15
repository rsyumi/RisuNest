import { describe, expect, it } from 'vitest'
import type { character, groupChat } from './database.svelte'
import { createCatalogCharacterStub } from './workingSetCatalog'
import {
    archivedAt,
    archivedConversationCount,
    characterIsArchived,
    countBlockedGroupMembers,
    formatArchivedAt,
} from './characterArchiveView'

function archivedStub(id: string): character | groupChat {
    return createCatalogCharacterStub({
        id,
        name: id,
        configuredIndex: 0,
        recentAt: 0,
        trashed: false,
        conversationCount: 0,
        type: 'character',
        archived: { archivedAt: 1_758_000_000_000, conversationCount: 12, messageCount: 412 },
    }) as character
}

function activeStub(id: string): character | groupChat {
    return createCatalogCharacterStub({
        id,
        name: id,
        configuredIndex: 1,
        recentAt: 0,
        trashed: false,
        conversationCount: 3,
        type: 'character',
    }) as character
}

describe('character archive view helpers', () => {
    it('reads the archived counts and time from the list stub', () => {
        const stub = archivedStub('archived')

        expect(characterIsArchived(stub)).toBe(true)
        expect(archivedConversationCount(stub)).toBe(12)
        expect(archivedAt(stub)).toBe(1_758_000_000_000)
        expect(formatArchivedAt(archivedAt(stub))).not.toBe('')
        expect(formatArchivedAt(undefined)).toBe('')
    })

    it('reports an active character as usable', () => {
        const stub = activeStub('active')

        expect(characterIsArchived(stub)).toBe(false)
        expect(archivedAt(stub)).toBeUndefined()
    })

    it('counts the archived members a group cannot use', () => {
        const characters = [archivedStub('a'), activeStub('b'), archivedStub('c')]
        const group = { chaId: 'group', characters: ['a', 'b', 'c'] } as unknown as groupChat

        expect(countBlockedGroupMembers(group, characters)).toBe(2)
    })

    it('counts nothing when every member is usable', () => {
        const characters = [activeStub('a'), activeStub('b')]
        const group = { chaId: 'group', characters: ['a', 'b'] } as unknown as groupChat

        expect(countBlockedGroupMembers(group, characters)).toBe(0)
    })
})
