import { describe, expect, test } from 'vitest'
import { collectMigrationReferenceGraph } from './migrationReferenceGraph'
import { inlayTokenRegex } from '../util/inlayTokens'

describe('migration reference graph', () => {
    test('collects sorted unique assets, inlays, and cold keys from database and cold characters', () => {
        inlayTokenRegex.lastIndex = 11
        const database = {
            customBackground: 'assets/z.png',
            userIcon: 'https://example.invalid/user.png',
            characterOrder: [{ name: 'Folder', imgFile: 'assets/' }],
            characters: [{
                chaId: 'resident',
                image: 'assets/a.png',
                coldstorage: 'cold-z',
                coldStoragedChats: ['cold-a', 'cold-z'],
                chats: [{ message: [{ data: '{{inlay::db-z}} {{inlayed::shared}} built-in.png' }] }],
            }],
            nested: { repeated: '{{inlayeddata::db-a}} {{inlay::shared}}' },
        } as any
        const coldValues = new Map<string, unknown>([
            ['cold-z', {
                character: {
                    type: 'character',
                    image: 'assets/cold.png',
                    additionalAssets: [['sound', 'assets/cold.mp3']],
                    chats: [{ message: [{ data: '{{inlay::cold-only}}' }] }],
                },
            }],
            ['cold-a', { message: [{ data: '{{inlay::cold-chat}} assets/not-a-projection.png' }] }],
        ])

        expect(collectMigrationReferenceGraph(database, coldValues)).toEqual({
            assets: ['assets/a.png', 'assets/cold.mp3', 'assets/cold.png', 'assets/z.png'],
            inlays: ['cold-chat', 'cold-only', 'db-a', 'db-z', 'shared'],
            cold: ['cold-a', 'cold-z'],
        })
        expect(inlayTokenRegex.lastIndex).toBe(11)
    })

    test('ignores empty assets, URLs, sentinels, unrelated strings, and resets regex per string', () => {
        const database = {
            customBackground: 'https://example.invalid/a.png',
            userIcon: 'assets/',
            characters: [{
                image: 'ccdefault:',
                chats: [{ message: [
                    { data: '{{inlay::first}}' },
                    { data: '{{inlay::second}}' },
                    { data: 'inlay::not-a-token assets/not-a-projection.png' },
                ] }],
            }],
        } as any

        expect(collectMigrationReferenceGraph(database, new Map())).toEqual({
            assets: [],
            inlays: ['first', 'second'],
            cold: [],
        })
    })
})
