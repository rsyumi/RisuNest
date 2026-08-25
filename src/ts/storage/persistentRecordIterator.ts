import type { Chat } from './database.svelte'
import type {
    CharacterDetail,
    CharacterSummary,
    ConversationSummary,
    DataRevision,
    PersistentRevisionReader,
} from './persistentDataStore'

const PINNED_RECORD_PAGE_SIZE = 128

export interface PinnedCharacterRecord {
    summary: CharacterSummary
    detail: CharacterDetail
}

export interface PinnedConversationRecord {
    summary: ConversationSummary
    value: Chat
}

export function assertPinnedRevision(
    expected: DataRevision,
    actual: DataRevision,
    description: string,
): void {
    if (actual !== expected) {
        throw new Error(`${description} returned revision ${actual}, expected ${expected}`)
    }
}

async function* iterateCharacterSummaries(
    reader: PersistentRevisionReader,
    trash: boolean,
): AsyncGenerator<CharacterSummary> {
    let cursor: string | undefined
    do {
        const page = await reader.queryCharacters({
            order: 'configured',
            trash,
            limit: PINNED_RECORD_PAGE_SIZE,
            cursor,
        })
        assertPinnedRevision(reader.revision, page.revision, 'Character page')
        for (const summary of page.items) yield summary
        cursor = page.nextCursor
    } while (cursor !== undefined)
}

export async function* iteratePinnedCharacterSummaries(
    reader: PersistentRevisionReader,
): AsyncGenerator<CharacterSummary> {
    const active = iterateCharacterSummaries(reader, false)[Symbol.asyncIterator]()
    const trash = iterateCharacterSummaries(reader, true)[Symbol.asyncIterator]()
    let activeValue = await active.next()
    let trashValue = await trash.next()
    while (!activeValue.done || !trashValue.done) {
        if (
            trashValue.done
            || (!activeValue.done
                && activeValue.value.configuredIndex <= trashValue.value.configuredIndex)
        ) {
            yield activeValue.value
            activeValue = await active.next()
        } else {
            yield trashValue.value
            trashValue = await trash.next()
        }
    }
}

export async function collectPinnedCharacterIds(
    reader: PersistentRevisionReader,
): Promise<string[]> {
    const ids: string[] = []
    for await (const summary of iteratePinnedCharacterSummaries(reader)) ids.push(summary.id)
    return ids
}

export async function countPinnedCharacters(
    reader: PersistentRevisionReader,
): Promise<number> {
    let count = 0
    for await (const _summary of iteratePinnedCharacterSummaries(reader)) count += 1
    return count
}

export async function* iteratePinnedCharacters(
    reader: PersistentRevisionReader,
): AsyncGenerator<PinnedCharacterRecord> {
    for await (const summary of iteratePinnedCharacterSummaries(reader)) {
        const detail = await reader.readCharacter(summary.id)
        if (!detail) throw new Error(`Missing character detail for ${summary.id}`)
        assertPinnedRevision(reader.revision, detail.revision, `Character ${summary.id}`)
        yield { summary, detail: detail.value }
    }
}

export async function* iteratePinnedConversations(
    reader: PersistentRevisionReader,
    characterId: string,
): AsyncGenerator<PinnedConversationRecord> {
    let cursor: string | undefined
    do {
        const page = await reader.queryConversations({
            characterId,
            order: 'configured',
            limit: PINNED_RECORD_PAGE_SIZE,
            cursor,
        })
        assertPinnedRevision(reader.revision, page.revision, `Conversation page for ${characterId}`)
        for (const summary of page.items) {
            const conversation = await reader.readConversation(characterId, summary.id)
            if (!conversation) throw new Error(`Missing conversation ${summary.id}`)
            assertPinnedRevision(
                reader.revision,
                conversation.revision,
                `Conversation ${summary.id}`,
            )
            yield { summary, value: conversation.value }
        }
        cursor = page.nextCursor
    } while (cursor !== undefined)
}
