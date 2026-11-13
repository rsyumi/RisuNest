import type { Chat, ChatFolder } from 'src/ts/storage/database.svelte'

export type IndexedChatRow = {
    chat: Chat
    index: number
}

export type IndexedChatFolder = {
    folder: ChatFolder
    index: number
    chats: IndexedChatRow[]
}

export function indexSideChatListRows(
    chats: Chat[],
    folders: ChatFolder[],
): {
    folders: IndexedChatFolder[]
    ungrouped: IndexedChatRow[]
} {
    const indexedFolders = folders.map((folder, index) => ({
        folder,
        index,
        chats: [] as IndexedChatRow[],
    }))
    const foldersById = new Map<string, IndexedChatFolder>()
    for (const folder of indexedFolders) {
        if (!foldersById.has(folder.folder.id)) {
            foldersById.set(folder.folder.id, folder)
        }
    }

    const ungrouped: IndexedChatRow[] = []
    chats.forEach((chat, index) => {
        const row = { chat, index }
        const folder =
            chat.folderId == null ? undefined : foldersById.get(chat.folderId)
        if (folder) folder.chats.push(row)
        else ungrouped.push(row)
    })

    return { folders: indexedFolders, ungrouped }
}

export type DroppedChatRow = {
    id: string
    /** A folder id moves the chat into it, null takes it out, undefined keeps it. */
    folderId?: string | null
}

/**
 * Rebuilds the chat list from rows read at drop time. Returns null when the rows
 * no longer describe exactly the current chats.
 */
export function orderChatsByDroppedRows(
    chats: Chat[],
    rows: DroppedChatRow[],
): Chat[] | null {
    const byId = new Map(chats.map((chat) => [chat.id, chat]))
    if (byId.size !== chats.length || rows.length !== chats.length) return null
    const ordered: Chat[] = []
    for (const row of rows) {
        const chat = byId.get(row.id)
        if (!chat) return null
        byId.delete(row.id)
        ordered.push(chat)
    }
    rows.forEach((row, index) => {
        const chat = ordered[index]
        if (typeof row.folderId === 'string') chat.folderId = row.folderId
        else if (row.folderId === null && chat.folderId != null) chat.folderId = null
    })
    return ordered
}

/**
 * Rows for a chat dropped on a folder itself rather than between its chats: the
 * chat becomes the first row of that folder.
 */
export function moveDroppedChatToFolderTop(
    rows: DroppedChatRow[],
    chatId: string,
    folderId: string,
): DroppedChatRow[] {
    if (!rows.some((row) => row.id === chatId)) return rows
    const others = rows.filter((row) => row.id !== chatId)
    const firstInFolder = others.findIndex((row) => row.folderId === folderId)
    others.splice(firstInFolder === -1 ? 0 : firstInFolder, 0, { id: chatId, folderId })
    return others
}

/** Reorders folders by id. Returns null when the ids no longer match. */
export function orderFoldersByDroppedIds(
    folders: ChatFolder[],
    ids: string[],
): ChatFolder[] | null {
    const byId = new Map(folders.map((folder) => [folder.id, folder]))
    if (byId.size !== folders.length || ids.length !== folders.length) return null
    const ordered: ChatFolder[] = []
    for (const id of ids) {
        const folder = byId.get(id)
        if (!folder) return null
        byId.delete(id)
        ordered.push(folder)
    }
    return ordered
}
