import {
    capturePersistentMutationToken,
    getPersistentDataRuntime,
} from '../storage/persistentDataRuntime.svelte'
import type {
    DataRevision,
    PersistentDataStore,
    PersistentRevisionReader,
} from '../storage/persistentDataStore'
import {
    assertPinnedRevision,
    withPersistentRevisionLease,
} from '../storage/persistentRecordIterator'

const BRANCH_SCAN_PAGE_SIZE = 128

type ChatBranch = {
    children: Map<string, ChatBranch>
    maxChildren: number
    chatId: number
    preview: string
    sourceConversationId: string
    sourceIndex: number | null
}

export type RenderedBranch = {
    x: number
    y: number
    connectX: number
    connectY: number
    content: string
    preview: string
    multiChild: boolean
    chatId: number
    sourceConversationId: string
    sourceIndex: number | null
    revision: DataRevision
}

export interface PinnedChatBranchGraph {
    revision: DataRevision
    branches: RenderedBranch[]
}

export interface PinnedChatBranchScanOptions {
    signal?: AbortSignal
}

function simpleHasher(str: string): string {
    let hash = 0
    if (str.length === 0) return ''
    for (let index = 0; index < str.length; index++) {
        const character = str.charCodeAt(index)
        hash = ((hash << 5) - hash) + character
        hash &= hash
    }
    return hash.toString(36)
}

function insertBranch(
    parent: ChatBranch,
    content: string,
    chatId: number,
    preview: string,
    sourceConversationId: string,
    sourceIndex: number | null,
): ChatBranch {
    let child = parent.children.get(content)
    if (!child) {
        child = {
            children: new Map(),
            maxChildren: 0,
            chatId,
            preview,
            sourceConversationId,
            sourceIndex,
        }
        parent.children.set(content, child)
    }
    return child
}

function calculateWidths(root: ChatBranch): void {
    const widths = new WeakMap<ChatBranch, number>()
    const stack: Array<{ node: ChatBranch; visited: boolean }> = [{ node: root, visited: false }]
    while (stack.length > 0) {
        const entry = stack.pop()!
        if (!entry.visited) {
            stack.push({ node: entry.node, visited: true })
            for (const child of entry.node.children.values()) {
                stack.push({ node: child, visited: false })
            }
            continue
        }
        if (entry.node.children.size === 0) {
            widths.set(entry.node, 1)
            continue
        }
        let width = 0
        for (const child of entry.node.children.values()) width += widths.get(child)!
        entry.node.maxChildren = width
        widths.set(entry.node, width)
    }
}

function renderBranches(root: ChatBranch, revision: DataRevision): RenderedBranch[] {
    const rendered: RenderedBranch[] = []
    type Frame = {
        node: ChatBranch
        children: Array<[string, ChatBranch]>
        childIndex: number
        x: number
        y: number
        connectX: number
        connectY: number
    }
    const stack: Frame[] = [{
        node: root,
        children: [...root.children],
        childIndex: 0,
        x: 0,
        y: 0,
        connectX: -1,
        connectY: -1,
    }]
    while (stack.length > 0) {
        const frame = stack.at(-1)!
        if (frame.childIndex >= frame.children.length) {
            stack.pop()
            continue
        }
        const [content, child] = frame.children[frame.childIndex++]
        const childX = frame.x
        rendered.push({
            x: childX,
            y: frame.y,
            content,
            connectX: frame.connectX,
            connectY: frame.connectY,
            multiChild: frame.node.children.size > 1,
            chatId: child.chatId,
            preview: child.preview,
            sourceConversationId: child.sourceConversationId,
            sourceIndex: child.sourceIndex,
            revision,
        })
        frame.x += child.maxChildren
        stack.push({
            node: child,
            children: [...child.children],
            childIndex: 0,
            x: childX,
            y: frame.y + 1,
            connectX: childX,
            connectY: frame.y,
        })
    }
    return rendered
}

async function scanPinnedGraph(
    reader: PersistentRevisionReader,
    characterId: string,
    signal?: AbortSignal,
): Promise<PinnedChatBranchGraph> {
    signal?.throwIfAborted()
    const character = await reader.readCharacter(characterId)
    signal?.throwIfAborted()
    if (!character) return { revision: reader.revision, branches: [] }
    assertPinnedRevision(reader.revision, character.revision, `Character ${characterId}`)
    if (character.value.chaId !== characterId) {
        throw new Error(`Character ${characterId} returned mismatched detail`)
    }
    const root: ChatBranch = {
        children: new Map(),
        maxChildren: 0,
        chatId: -1,
        preview: '',
        sourceConversationId: '',
        sourceIndex: null,
    }
    let cursor: string | undefined
    let chatId = 0
    do {
        signal?.throwIfAborted()
        const page = await reader.queryConversations({
            characterId,
            order: 'configured',
            limit: BRANCH_SCAN_PAGE_SIZE,
            ...(cursor === undefined ? {} : { cursor }),
        })
        signal?.throwIfAborted()
        assertPinnedRevision(reader.revision, page.revision, `Conversation page for ${characterId}`)
        for (const summary of page.items) {
            signal?.throwIfAborted()
            if (summary.characterId !== characterId) {
                throw new Error(`Conversation ${summary.id} returned mismatched character ID`)
            }
            const firstMessage = summary.fmIndex === -1
                ? character.value.firstMessage ?? ''
                : character.value.alternateGreetings?.[summary.fmIndex ?? 0] ?? ''
            let branch = insertBranch(
                root,
                simpleHasher(firstMessage),
                chatId,
                firstMessage,
                summary.id,
                null,
            )
            for (let startIndex = 0; startIndex < summary.messageCount;) {
                signal?.throwIfAborted()
                const result = await reader.readConversationWindow({
                    characterId,
                    conversationId: summary.id,
                    startIndex,
                    limit: Math.min(BRANCH_SCAN_PAGE_SIZE, summary.messageCount - startIndex),
                })
                signal?.throwIfAborted()
                if (!result) throw new Error(`Missing conversation ${summary.id}`)
                assertPinnedRevision(reader.revision, result.revision, `Conversation ${summary.id}`)
                const window = result.value
                if (
                    window.characterId !== characterId
                    || window.conversationId !== summary.id
                    || window.startIndex !== startIndex
                    || window.endIndex !== startIndex + window.messages.length
                    || window.totalMessages !== summary.messageCount
                    || window.messages.length === 0
                ) {
                    throw new Error(`Conversation ${summary.id} returned mismatched range evidence`)
                }
                for (let offset = 0; offset < window.messages.length; offset++) {
                    const sourceIndex = window.startIndex + offset
                    const message = window.messages[offset]
                    branch = insertBranch(
                        branch,
                        simpleHasher(message.data),
                        chatId,
                        message.data,
                        summary.id,
                        sourceIndex,
                    )
                }
                startIndex = window.endIndex
            }
            chatId++
        }
        cursor = page.nextCursor
    } while (cursor !== undefined)

    signal?.throwIfAborted()
    calculateWidths(root)
    return {
        revision: reader.revision,
        branches: renderBranches(root, reader.revision),
    }
}

export async function scanPinnedChatBranches(
    store: PersistentDataStore,
    characterId: string,
    revision: DataRevision,
    options: PinnedChatBranchScanOptions = {},
): Promise<PinnedChatBranchGraph> {
    options.signal?.throwIfAborted()
    const lease = await store.acquireRevision(revision)
    return withPersistentRevisionLease(lease, (reader) =>
        scanPinnedGraph(reader, characterId, options.signal))
}

export async function getChatBranches(characterId: string): Promise<RenderedBranch[]> {
    const token = await capturePersistentMutationToken('chat-branches')
    const result = await scanPinnedChatBranches(
        getPersistentDataRuntime().store,
        characterId,
        token.revision,
    )
    return result.branches
}
