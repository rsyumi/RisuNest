import { describe, expect, it } from 'vitest'
import { buildChatViewport } from './chatViewport'

function keys(count: number): string[] {
    return Array.from({ length: count }, (_, index) => `message-${index}`)
}

describe('buildChatViewport', () => {
    it('uses a bounded key source and indexed height corrections without scanning omitted rows', () => {
        let keyReads = 0
        let indexLookups = 0
        const viewport = buildChatViewport({
            keySource: {
                length: 10_000,
                keyAt(index) {
                    keyReads += 1
                    return `message-${index}`
                },
                indexOf(key) {
                    indexLookups += 1
                    return Number(key.slice('message-'.length))
                },
            },
            budget: 64,
            overscan: 8,
            estimatedMessageHeight: 100,
            measuredHeightsByIndex: new Map([
                [0, 150],
                [5_000, 80],
            ]),
            anchor: { key: 'message-5000', indexHint: 5_000, relativeOffset: 17 },
            pins: [{ key: 'message-5000', reason: 'editor', indexHint: 5_000 }],
        })

        expect(viewport.messageRows).toHaveLength(64)
        expect(viewport.messageRows.some((row) => row.index === 5_000)).toBe(true)
        expect(viewport.rows[0]).toEqual({
            kind: 'gap',
            startIndex: 0,
            endIndex: 4_992,
            height: 499_250,
        })
        expect(keyReads).toBeLessThanOrEqual(66)
        expect(indexLookups).toBe(0)
    })

    it('starts at the newest tail and counts overscan inside the mounted budget', () => {
        const normal = buildChatViewport({
            keys: keys(100),
            budget: 64,
            overscan: 8,
            estimatedMessageHeight: 100,
        })
        const lowSpec = buildChatViewport({
            keys: keys(100),
            budget: 40,
            overscan: 8,
            estimatedMessageHeight: 100,
        })

        expect(normal.messageRows.map((row) => row.index)).toEqual(
            Array.from({ length: 64 }, (_, index) => index + 36),
        )
        expect(normal.mountedMessageCount).toBe(64)
        expect(lowSpec.messageRows.map((row) => row.index)).toEqual(
            Array.from({ length: 40 }, (_, index) => index + 60),
        )
        expect(lowSpec.mountedMessageCount).toBe(40)
    })

    it('jumps directly within 10,000 messages and rejects out-of-range targets without moving the anchor', () => {
        const messageKeys = keys(10_000)
        const anchor = { key: 'message-9000', indexHint: 9000, relativeOffset: 17 }
        const jumped = buildChatViewport({
            keys: messageKeys,
            budget: 64,
            overscan: 8,
            estimatedMessageHeight: 100,
            anchor,
            jumpTarget: 100,
        })

        expect(jumped.jumpAccepted).toBe(true)
        expect(jumped.messageRows.some((row) => row.index === 100)).toBe(true)
        expect(jumped.messageRows).toHaveLength(64)
        expect(jumped.messageRows[0].index).toBe(92)
        expect(jumped.messageRows.some((row) => row.index === 9000)).toBe(false)

        const rejected = buildChatViewport({
            keys: messageKeys,
            budget: 64,
            overscan: 8,
            estimatedMessageHeight: 100,
            anchor,
            jumpTarget: 10_000,
        })

        expect(rejected.jumpAccepted).toBe(false)
        expect(rejected.anchor).toEqual(anchor)
        expect(rejected.messageRows.some((row) => row.index === 9000)).toBe(true)
    })

    it('returns measured gap rows while preserving the stable anchor and relative offset across height updates', () => {
        const anchor = { key: 'message-5', indexHint: 5, relativeOffset: 23 }
        const before = buildChatViewport({
            keys: keys(10),
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 100,
            measuredHeights: new Map([['message-1', 150]]),
            anchor,
        })
        const after = buildChatViewport({
            keys: keys(10),
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 100,
            measuredHeights: new Map([['message-1', 250]]),
            anchor: before.anchor,
        })

        expect(before.rows).toEqual([
            { kind: 'gap', startIndex: 0, endIndex: 4, height: 450 },
            { kind: 'message', index: 4, key: 'message-4', pinReasons: [] },
            { kind: 'message', index: 5, key: 'message-5', pinReasons: [] },
            { kind: 'message', index: 6, key: 'message-6', pinReasons: [] },
            { kind: 'gap', startIndex: 7, endIndex: 10, height: 300 },
        ])
        expect(after.rows[0]).toEqual({ kind: 'gap', startIndex: 0, endIndex: 4, height: 550 })
        expect(after.anchor).toEqual(anchor)
    })

    it('keeps disjoint streaming, editor, and playing-media pins inside the budget with independent reasons', () => {
        const viewport = buildChatViewport({
            keys: keys(20),
            budget: 6,
            overscan: 1,
            estimatedMessageHeight: 10,
            anchor: { key: 'message-10', indexHint: 10, relativeOffset: 0 },
            pins: [
                { key: 'message-1', reason: 'editor' },
                { key: 'message-18', reason: 'playing-media' },
                { key: 'message-18', reason: 'editor' },
                { key: 'message-19', reason: 'streaming' },
            ],
        })

        expect(viewport.messageRows.map((row) => row.index)).toEqual([1, 9, 10, 11, 18, 19])
        expect(viewport.messageRows.find((row) => row.index === 18)?.pinReasons).toEqual([
            'playing-media',
            'editor',
        ])
        expect(viewport.rows.filter((row) => row.kind === 'gap')).toHaveLength(3)
        expect(viewport.mountedMessageCount).toBe(6)
        expect(viewport.pinOverflow).toBeNull()
    })

    it('keeps every active pin, including the newest streaming row, and reports explicit overflow', () => {
        const viewport = buildChatViewport({
            keys: keys(10),
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 10,
            anchor: { key: 'message-5', indexHint: 5, relativeOffset: 0 },
            pins: [
                { key: 'message-0', reason: 'editor' },
                { key: 'message-2', reason: 'playing-media' },
                { key: 'message-7', reason: 'playing-media' },
                { key: 'message-9', reason: 'streaming' },
            ],
        })

        expect(viewport.messageRows.map((row) => row.index)).toEqual([0, 2, 7, 9])
        expect(viewport.messageRows.at(-1)).toMatchObject({ index: 9, pinReasons: ['streaming'] })
        expect(viewport.mountedMessageCount).toBe(4)
        expect(viewport.pinOverflow).toEqual({
            count: 1,
            pinnedMessageCount: 4,
            pinnedKeys: ['message-0', 'message-2', 'message-7', 'message-9'],
        })
    })

    it('tracks a logical anchor through inserts and rerolls, then falls back deterministically after deletion', () => {
        const anchor = { key: 'c', indexHint: 2, relativeOffset: 11 }
        const inserted = buildChatViewport({
            keys: ['a', 'x', 'b', 'c', 'd', 'e'],
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 10,
            anchor,
        })
        const rerolled = buildChatViewport({
            keys: ['a', 'x', 'b', 'c', 'd', 'e'],
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 10,
            anchor: inserted.anchor,
        })
        const deleted = buildChatViewport({
            keys: ['a', 'x', 'b', 'd', 'e'],
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 10,
            anchor: rerolled.anchor,
        })

        expect(inserted.anchor).toEqual({ key: 'c', indexHint: 3, relativeOffset: 11 })
        expect(rerolled.anchor).toEqual(inserted.anchor)
        expect(deleted.anchor).toEqual({ key: 'd', indexHint: 3, relativeOffset: 11 })
    })
})
