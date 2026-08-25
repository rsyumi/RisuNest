import { describe, expect, it } from 'vitest'
import type { Chat, Database, character } from './database.svelte'
import { captureResidentPersistentCharacter } from './persistentDataRuntime'
import {
    isConversationSummaryStub,
    WorkingSetResidencyRegistry,
} from './workingSetResidency'
import {
    createCatalogCharacterStub,
    createCatalogPresetWorkingSet,
    getCatalogCharacterMetadata,
    hasIncompletePersistentWorkingSet,
    hydrateWorkingSetCharacterDetail,
    isCatalogCharacterStub,
} from './workingSetCatalog'

function chat(id: string, options: { streaming?: boolean; empty?: boolean } = {}): Chat {
    return {
        id,
        name: id,
        note: '',
        localLore: [],
        isStreaming: options.streaming,
        message: options.empty ? [] : [{ role: 'user', data: id }],
    } as Chat
}

function characterWithChats(chats: Chat[]): character {
    return {
        type: 'character',
        chaId: 'char-a',
        name: 'Character A',
        chats,
    } as character
}

describe('WorkingSetResidencyRegistry', () => {
    it('pins the selected conversation while releasing a nonselected body to its summary', () => {
        const registry = new WorkingSetResidencyRegistry()
        const selected = chat('selected')
        const inactive = chat('inactive')
        const value = characterWithChats([selected, inactive])
        registry.pinSelectedConversation('char-a', 'selected')

        expect(registry.releaseConversationToSummary(value, 'selected')).toBe(false)
        expect(registry.releaseConversationToSummary(value, 'inactive')).toBe(true)

        expect(value.chats[0]).toBe(selected)
        expect(value.chats[0].message).toEqual([{ role: 'user', data: 'selected' }])
        expect(isConversationSummaryStub(value.chats[1])).toBe(true)
        expect(value.chats[1]).toMatchObject({ id: 'inactive', name: 'inactive', message: [] })
    })

    it('keeps only the latest character detail resident across repeated visits', () => {
        const registry = new WorkingSetResidencyRegistry()
        const database = {
            characters: ['a', 'b', 'c'].map((id) => ({
                type: 'character',
                chaId: `char-${id}`,
                name: id.toUpperCase(),
                creatorNotes: `notes-${id}`,
                personality: `body-${id}`,
                chats: [chat(`chat-${id}`)],
            })),
        } as unknown as Database

        expect(registry.releaseCharacterToCatalog(database, 'char-a')).toBe(true)
        expect(registry.releaseCharacterToCatalog(database, 'char-b')).toBe(true)

        expect(database.characters.map(isCatalogCharacterStub)).toEqual([true, true, false])
        expect(database.characters[0]).not.toHaveProperty('personality')
        expect(database.characters[1]).not.toHaveProperty('personality')
        expect(database.characters[2]).toHaveProperty('personality', 'body-c')
        expect(getCatalogCharacterMetadata(database.characters[0])).toMatchObject({
            configuredIndex: 0,
            conversationCount: 1,
        })
        expect(getCatalogCharacterMetadata(database.characters[1])).toMatchObject({
            configuredIndex: 1,
            conversationCount: 1,
        })
    })

    it('releases full detail even when the character has no chats', () => {
        const registry = new WorkingSetResidencyRegistry()
        const database = {
            characters: [{
                type: 'character',
                chaId: 'char-empty',
                name: 'Empty',
                personality: 'must be released',
                chats: [],
            }],
        } as unknown as Database

        expect(registry.releaseCharacterToCatalog(database, 'char-empty')).toBe(true)

        expect(isCatalogCharacterStub(database.characters[0])).toBe(true)
        expect(database.characters[0]).not.toHaveProperty('personality')
        expect(getCatalogCharacterMetadata(database.characters[0])?.conversationCount).toBe(0)
        expect(registry.isCharacterReleased('char-empty')).toBe(true)
    })

    it('releases group-member detail and reloads fresh detail on revisit', () => {
        const registry = new WorkingSetResidencyRegistry()
        const catalog = createCatalogCharacterStub({
            id: 'member-a',
            name: 'Member',
            configuredIndex: 3,
            recentAt: 0,
            trashed: false,
            conversationCount: 12,
            type: 'character',
        })
        const database = { characters: [catalog] } as unknown as Database
        const firstVisit = hydrateWorkingSetCharacterDetail(database, 0, {
            type: 'character',
            chaId: 'member-a',
            name: 'Member',
            personality: 'first personality',
            globalLore: [{ key: 'first lore', content: 'first' }],
        } as any) as character
        registry.markCharacterHydrated('member-a')

        expect(registry.releaseCharacterToCatalog(database, 'member-a')).toBe(true)
        expect(isCatalogCharacterStub(database.characters[0])).toBe(true)
        expect(database.characters[0]).not.toHaveProperty('personality')
        expect(database.characters[0]).not.toHaveProperty('globalLore')
        expect(getCatalogCharacterMetadata(database.characters[0])).toMatchObject({
            configuredIndex: 3,
            conversationCount: 12,
        })

        const secondVisit = hydrateWorkingSetCharacterDetail(database, 0, {
            type: 'character',
            chaId: 'member-a',
            name: 'Member reloaded',
            personality: 'second personality',
            globalLore: [{ key: 'second lore', content: 'second' }],
        } as any) as character
        registry.markCharacterHydrated('member-a')

        expect(secondVisit).not.toBe(firstVisit)
        expect(isCatalogCharacterStub(secondVisit)).toBe(false)
        expect(registry.isCharacterReleased('member-a')).toBe(false)
        expect(secondVisit).toMatchObject({
            name: 'Member reloaded',
            personality: 'second personality',
            globalLore: [{ key: 'second lore', content: 'second' }],
        })
    })

    it('keeps a streaming character fully resident', () => {
        const registry = new WorkingSetResidencyRegistry()
        const value = characterWithChats([chat('streaming', { streaming: true })])
        const database = { characters: [value] } as unknown as Database

        expect(registry.releaseCharacterToCatalog(database, 'char-a')).toBe(false)

        expect(database.characters[0]).toBe(value)
        expect(isCatalogCharacterStub(database.characters[0])).toBe(false)
        expect(registry.isCharacterReleased('char-a')).toBe(false)
    })

    it('keeps complete detail when maximum compatibility disables eviction', () => {
        const registry = new WorkingSetResidencyRegistry()
        const value = characterWithChats([chat('settled')])
        const database = { characters: [value] } as unknown as Database
        registry.setEvictionAllowed(false)

        expect(registry.releaseCharacterToCatalog(database, 'char-a')).toBe(false)

        expect(database.characters[0]).toBe(value)
        expect(isCatalogCharacterStub(database.characters[0])).toBe(false)
        expect(registry.isCharacterReleased('char-a')).toBe(false)
    })

    it('keeps the entire character resident while any conversation is streaming', () => {
        const registry = new WorkingSetResidencyRegistry()
        const settled = chat('settled')
        const streaming = chat('streaming', { streaming: true })
        const alreadyEmpty = chat('empty', { empty: true })
        const value = characterWithChats([settled, streaming, alreadyEmpty])
        const chatKeys = value.chats.map((entry) => Object.keys(entry).sort())

        expect(registry.releaseCharacterMessages(value)).toBe(false)

        expect(settled.message).toEqual([{ role: 'user', data: 'settled' }])
        expect(streaming.message).toEqual([{ role: 'user', data: 'streaming' }])
        expect(alreadyEmpty.message).toEqual([])
        expect(value.chats.map((entry) => Object.keys(entry).sort())).toEqual(chatKeys)
        expect(registry.isCharacterReleased('char-a')).toBe(false)
    })

    it('blocks release immediately when eviction is disabled', () => {
        const registry = new WorkingSetResidencyRegistry()
        const value = characterWithChats([chat('settled')])

        registry.setEvictionAllowed(false)

        expect(registry.allowsEviction).toBe(false)
        expect(registry.releaseCharacterMessages(value)).toBe(false)
        expect(value.chats[0].message).toEqual([{ role: 'user', data: 'settled' }])
        expect(registry.isCharacterReleased('char-a')).toBe(false)
    })

    it('tracks boot catalog stubs by stable ID until exact hydration replaces them', () => {
        const registry = new WorkingSetResidencyRegistry()

        registry.markCharacterReleased('char-a')
        expect(registry.isCharacterReleased('char-a')).toBe(true)

        registry.markCharacterHydrated('char-a')
        expect(registry.isCharacterReleased('char-a')).toBe(false)
    })

    it('excludes released empty arrays from persistence capture', () => {
        const registry = new WorkingSetResidencyRegistry()
        const value = characterWithChats([chat('settled')])
        const database = { characters: [value] } as never

        registry.releaseCharacterMessages(value)

        expect(captureResidentPersistentCharacter(database, 'char-a', registry)).toBeNull()
        registry.markCharacterHydrated('char-a')
        expect(captureResidentPersistentCharacter(database, 'char-a', registry)).toBe(value)
    })

    it('excludes a fresh catalog stub even before the residency registry is populated', () => {
        const registry = new WorkingSetResidencyRegistry()
        const stub = createCatalogCharacterStub({
            id: 'char-a',
            type: 'character',
            name: 'Character A',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 3,
        })
        const database = { characters: [stub] } as never

        expect(captureResidentPersistentCharacter(database, 'char-a', registry)).toBeNull()
    })

    it('rejects a conversation-summary working set as a complete replacement source', () => {
        const registry = new WorkingSetResidencyRegistry()
        const complete = characterWithChats([chat('selected'), chat('inactive')])
        const database = {
            characters: [complete],
            botPresets: [],
        } as unknown as Database
        registry.pinSelectedConversation('char-a', 'selected')
        expect(registry.releaseConversationToSummary(complete, 'inactive')).toBe(true)

        expect(hasIncompletePersistentWorkingSet(database, registry)).toBe(true)
    })

    it('detects every structural form of an incomplete persistent working set', () => {
        const registry = new WorkingSetResidencyRegistry()
        const complete = characterWithChats([chat('settled')])
        const completeDatabase = {
            characters: [complete],
            botPresets: [{ name: 'Complete', mainPrompt: 'full' }],
        } as unknown as Database

        expect(hasIncompletePersistentWorkingSet(completeDatabase, registry)).toBe(false)

        registry.markCharacterReleased('char-a')
        expect(hasIncompletePersistentWorkingSet(completeDatabase, registry)).toBe(true)

        registry.markCharacterHydrated('char-a')
        const catalogCharacter = createCatalogCharacterStub({
            id: 'char-a',
            type: 'character',
            name: 'Character A',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 1,
        })
        expect(hasIncompletePersistentWorkingSet({
            ...completeDatabase,
            characters: [catalogCharacter],
        }, registry)).toBe(true)

        const catalogPresets = createCatalogPresetWorkingSet({
            revision: 1,
            items: [{ id: '0', configuredIndex: 0, name: 'Complete' }],
        }, {
            summary: { id: '0', configuredIndex: 0, name: 'Complete' },
            value: completeDatabase.botPresets[0],
        })
        expect(hasIncompletePersistentWorkingSet({
            ...completeDatabase,
            botPresets: catalogPresets,
        }, registry)).toBe(true)
    })
})
