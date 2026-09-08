import { describe, expect, it } from 'vitest'

import { indexSideChatListRows } from './sideChatListRows'

describe('side chat list row indexing', () => {
    it('keeps every chat once while preserving folder and chat order', () => {
        const folders = [
            { id: 'folder-b', name: 'B', folded: true },
            { id: 'folder-empty', name: 'Empty', folded: false },
            { id: '', name: 'Empty ID', folded: false },
            { id: 'folder-a', name: 'A', folded: false },
        ]
        const chat = (id: string, folderId: string | null) => ({
            id,
            folderId,
            message: [],
            note: '',
            name: id,
            localLore: [],
        })
        const chats = [
            chat('chat-a1', 'folder-a'),
            chat('chat-free', null),
            chat('chat-b1', 'folder-b'),
            chat('chat-orphan', 'missing-folder'),
            chat('chat-empty-id', ''),
            chat('chat-a2', 'folder-a'),
        ]

        const indexed = indexSideChatListRows(chats, folders)

        expect(
            indexed.folders.map((row) => ({
                id: row.folder.id,
                folded: row.folder.folded,
                chatIds: row.chats.map(({ chat }) => chat.id),
                indices: row.chats.map(({ index }) => index),
            })),
        ).toEqual([
            {
                id: 'folder-b',
                folded: true,
                chatIds: ['chat-b1'],
                indices: [2],
            },
            { id: 'folder-empty', folded: false, chatIds: [], indices: [] },
            { id: '', folded: false, chatIds: ['chat-empty-id'], indices: [4] },
            {
                id: 'folder-a',
                folded: false,
                chatIds: ['chat-a1', 'chat-a2'],
                indices: [0, 5],
            },
        ])
        expect(
            indexed.ungrouped.map(({ chat, index }) => ({
                id: chat.id,
                index,
            })),
        ).toEqual([
            { id: 'chat-free', index: 1 },
            { id: 'chat-orphan', index: 3 },
        ])
    })
})
