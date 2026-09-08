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
