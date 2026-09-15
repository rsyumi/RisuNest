import { describe, expect, it, vi } from 'vitest'
import { expandColdPayloads } from './coldPayloadExpansion'
import { coldStorageHeader } from './coldstorageData'

function placeholderChat(key: string) {
    return {
        id: `chat-${key}`,
        message: [{ time: 1, data: `${coldStorageHeader}${key}`, role: 'char' }],
        note: '',
        name: '',
        localLore: [],
    }
}

describe('expandColdPayloads', () => {
    it('restores an archived character and drops its reference', async () => {
        const db = {
            characters: [
                {
                    type: 'character',
                    chaId: 'cha-1',
                    name: 'Stub',
                    chats: [placeholderChat('chat-key')],
                    coldstorage: 'character-key',
                    coldStoragedChats: ['chat-key'],
                },
            ],
        } as any

        const read = vi.fn(async (key: string) => {
            if (key === 'character-key') {
                return {
                    character: {
                        type: 'character',
                        chaId: 'cha-1',
                        name: 'Restored',
                        chats: [placeholderChat('chat-key')],
                    },
                }
            }
            return { message: [{ time: 2, data: 'body', role: 'char' }], localLore: [] }
        })

        const result = await expandColdPayloads(db, read)

        expect(db.characters[0].name).toBe('Restored')
        expect(db.characters[0].chats[0].message[0].data).toBe('body')
        expect(db.characters[0].coldstorage).toBeUndefined()
        expect(db.characters[0].coldStoragedChats).toBeUndefined()
        expect(result.expandedKeys).toEqual(['character-key', 'chat-key'])
        expect(result.unavailableKeys).toEqual([])
    })

    it('accepts a bare message array payload', async () => {
        const db = { characters: [{ chaId: 'cha-1', chats: [placeholderChat('k')] }] } as any
        const messages = [{ time: 3, data: 'array body', role: 'char' }]

        await expandColdPayloads(db, async () => messages)

        expect(db.characters[0].chats[0].message).toEqual(messages)
    })

    it('keeps the record and reports the key when a payload is unavailable', async () => {
        const db = {
            characters: [
                {
                    chaId: 'cha-1',
                    name: 'Stub',
                    chats: [placeholderChat('missing-chat')],
                    coldstorage: 'missing-character',
                    coldStoragedChats: ['missing-chat'],
                },
            ],
        } as any

        const result = await expandColdPayloads(db, async () => null)

        expect(db.characters[0].name).toBe('Stub')
        expect(db.characters[0].chats[0].message[0].data).toBe(`${coldStorageHeader}missing-chat`)
        expect(db.characters[0].coldstorage).toBeUndefined()
        expect(result.expandedKeys).toEqual([])
        expect(result.unavailableKeys).toEqual(['missing-character', 'missing-chat'])
    })

    it('reports a read failure instead of throwing', async () => {
        const db = { characters: [{ chaId: 'cha-1', chats: [placeholderChat('boom')] }] } as any

        const result = await expandColdPayloads(db, async () => {
            throw new Error('unreachable')
        })

        expect(result.unavailableKeys).toEqual(['boom'])
    })

    it('rethrows an abort instead of marking every key unavailable', async () => {
        const db = {
            characters: [{ chaId: 'cha-1', chats: [placeholderChat('a'), placeholderChat('b')] }],
        } as any

        const read = vi.fn(async () => {
            throw new DOMException('aborted', 'AbortError')
        })

        await expect(expandColdPayloads(db, read)).rejects.toThrow('aborted')
        expect(read).toHaveBeenCalledTimes(1)
    })

    it('normalizes PocketRisu swipes while expanding', async () => {
        const db = { characters: [{ chaId: 'cha-1', chats: [placeholderChat('pocket')] }] } as any

        await expandColdPayloads(db, async () => ({
            message: [{ time: 4, data: 'b', role: 'char', swipes: ['a', 'b'], swipeId: 1 }],
        }))

        const message = db.characters[0].chats[0].message[0]
        expect(message.responseVariants.candidates).toHaveLength(2)
        expect(message.responseVariants.selectedId).toBe(message.responseVariants.candidates[1].id)
    })
})
