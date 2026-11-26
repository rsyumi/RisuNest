import { safeStructuredClone } from './safeStructuredClone'
import type { PersistentConversationWindowReader } from './conversationViewportSource'
import type { Message } from './storage/database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT, validateConversationWindowQuery, type ConversationWindowQuery,
    type ConversationWindow, type Versioned, type ConversationMessageMetadata } from './storage/persistentDataStore'
import type { WindowedOverlayProjection } from './storage/windowedConversationOverlay'

export function projectConversationWindows(reader: PersistentConversationWindowReader, projection: WindowedOverlayProjection,
    characterId: string, conversationId: string, storedCount: number,
    sameRevision?: (revision: number) => boolean): PersistentConversationWindowReader {
    const acceptsRevision = sameRevision ?? ((revision: number) => revision === projection.revision)
    const findAnchor = async (query: ConversationWindowQuery): Promise<number | null> => {
        const last = query.anchorOccurrence === 'last'
        const id = query.anchorMessageId!
        let candidate: number | null = null
        const include = (index: number) => {
            if (candidate === null || (last ? index > candidate : index < candidate)) candidate = index
        }
        for (const interval of projection.intervals(0, projection.count)) {
            if ('messages' in interval) interval.messages.forEach((message, offset) => {
                if (message.chatId === id) include(interval.start + offset)
            })
        }
        const occurrence = async (direction: 'first' | 'last') => {
            const match = await reader.readConversationWindow({ characterId, conversationId, anchorMessageId: id,
                anchorOccurrence: direction, before: 0, after: 0 })
            if (!match) return null
            if (!acceptsRevision(match.revision)) throw new DOMException('Conversation revision changed', 'AbortError')
            if (match.value.characterId !== characterId || match.value.conversationId !== conversationId ||
                match.value.totalMessages !== storedCount || match.value.messages.length !== 1 ||
                match.value.messages[0].chatId !== id) throw new Error('Conversation anchor returned a mismatched window')
            return match.value.startIndex
        }
        const first = await occurrence(last ? 'last' : 'first')
        if (first === null) return candidate
        const visible = projection.storedToVisible(first)
        const at = visible === null ? null : projection.row(visible)
        if (visible !== null && at && ('storedIndex' in at || at.message.chatId === id)) {
            include(visible)
            return candidate
        }
        // A deleted or renamed extreme can expose an interior duplicate.
        const other = await occurrence(last ? 'first' : 'last')
        if (other === null || other === first) return candidate
        let low = last ? other : first + 1
        let high = last ? first : other + 1
        while (low < high) {
            const limit = Math.min(64, high - low)
            const startIndex = last ? high - limit : low
            const page = await reader.readConversationWindow({ characterId, conversationId, startIndex, limit })
            if (!page || !acceptsRevision(page.revision)) throw new DOMException('Conversation revision changed', 'AbortError')
            if (page.value.characterId !== characterId || page.value.conversationId !== conversationId ||
                page.value.startIndex !== startIndex || page.value.totalMessages !== storedCount ||
                page.value.messages.length !== limit) throw new Error('Conversation anchor scan returned a mismatched window')
            for (let n = 0; n < limit; n++) {
                const offset = last ? limit - n - 1 : n
                if (page.value.messages[offset].chatId !== id) continue
                const index = projection.storedToVisible(startIndex + offset)
                if (index === null) continue
                const row = projection.row(index)
                if (row && 'storedIndex' in row) { include(index); return candidate }
            }
            if (last) high -= limit
            else low += limit
        }
        return candidate
    }
    const read = async <T extends Message | ConversationMessageMetadata>(
        query: ConversationWindowQuery,
        readStored: (input: ConversationWindowQuery) => Promise<Versioned<Omit<ConversationWindow, 'messages'> & { messages: T[] }> | null>,
        fromOverlay: (message: Message) => T,
    ) => {
        validateConversationWindowQuery(query)
        if (query.characterId !== characterId || query.conversationId !== conversationId) throw new RangeError('Expected the selected conversation')
        let startIndex: number, endIndex: number
        if (query.startIndex !== undefined) {
            startIndex = Math.min(query.startIndex, projection.count)
            endIndex = Math.min(startIndex + query.limit!, projection.count)
        } else if (query.anchorMessageId !== undefined) {
            const anchor = await findAnchor(query)
            if (anchor === null) return null
            startIndex = Math.max(0, anchor - Math.max(0, query.before ?? 0))
            endIndex = Math.min(projection.count, anchor + Math.max(0, query.after ?? 0) + 1)
        } else {
            endIndex = projection.count
            startIndex = Math.max(0, endIndex - Math.max(0, query.limit ?? 128))
        }
        const messages: T[] = []
        for (const interval of projection.intervals(startIndex, endIndex - startIndex)) {
            if ('messages' in interval) {
                for (const message of interval.messages) messages.push(fromOverlay(message))
                continue
            }
            for (let offset = 0; offset < interval.length; offset += CONVERSATION_RANGE_MAX_LIMIT) {
                const storedStart = interval.storedStart + offset
                const limit = Math.min(interval.length - offset, CONVERSATION_RANGE_MAX_LIMIT)
                const result = await readStored({ characterId, conversationId, skipParserWork: query.skipParserWork, startIndex: storedStart, limit })
                if (!result) return null
                if (!acceptsRevision(result.revision)) throw new DOMException('Conversation revision changed', 'AbortError')
                const value = result.value
                if (value.characterId !== characterId || value.conversationId !== conversationId ||
                    value.startIndex !== storedStart || value.endIndex !== storedStart + limit ||
                    value.totalMessages !== storedCount || value.messages.length !== limit) {
                    throw new Error(`Conversation ${conversationId} returned a mismatched window`)
                }
                messages.push(...value.messages)
            }
        }
        return { revision: projection.revision, value: {
            characterId, conversationId, startIndex, endIndex,
            totalMessages: projection.count, hasMoreBefore: startIndex > 0, hasMoreAfter: endIndex < projection.count, messages,
        } }
    }
    return {
        readConversationWindow: query => read(query, input => reader.readConversationWindow(input), safeStructuredClone<Message>),
        readConversationMessageMetadataWindow: reader.readConversationMessageMetadataWindow
            ? query => read<ConversationMessageMetadata>(query, input => reader.readConversationMessageMetadataWindow!(input),
                ({ chatId, role, disabled }) => ({ chatId, role, disabled, parserInert: false }))
            : undefined,
    }
}
