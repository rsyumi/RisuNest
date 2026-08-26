import { describe, expect, it } from 'vitest'

import type { Message } from './database.svelte'
import { SegmentedConversationResidency } from './segmentedConversationResidency'

function message(data: string): Message {
    return { role: 'user', data, chatId: `id-${data}` }
}

function createResidency(options: { totalMessages?: number; maxResidentBytes?: number } = {}) {
    return new SegmentedConversationResidency({
        revision: 7,
        totalMessages: options.totalMessages ?? 6,
        maxResidentBytes: options.maxResidentBytes ?? 64,
        measureMessage: () => 1,
    })
}

describe('SegmentedConversationResidency', () => {
    it('merges adjacent absolute ranges and returns detached snapshots plus missing intervals', () => {
        const residency = createResidency()
        const first = [message('zero'), message('one')]
        const second = [message('two'), message('three')]

        residency.storeRange({ revision: 7, startIndex: 0, totalMessages: 6, messages: first })
        residency.storeRange({ revision: 7, startIndex: 2, totalMessages: 6, messages: second })
        first[0].data = 'mutated input'

        expect(residency.residentIntervals).toEqual([{
            startIndex: 0,
            endIndex: 4,
            byteSize: 4,
            messages: [message('zero'), message('one'), message('two'), message('three')],
        }])
        const read = residency.readRange(1, 2)
        expect(read).toEqual([message('one'), message('two')])
        read![0].data = 'mutated output'
        expect(residency.readRange(1, 1)).toEqual([message('one')])
        expect(residency.missingRanges(0, 6)).toEqual([{ startIndex: 4, endIndex: 6 }])
    })

    it('evicts least-recent clean entries by byte budget but retains counted range pins', () => {
        const residency = createResidency({ totalMessages: 3, maxResidentBytes: 1 })
        const pinA = residency.pinRange(0, 2, 'viewport')
        const pinB = residency.pinRange(0, 1, 'viewport')

        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 3,
            messages: [message('zero'), message('one'), message('two')],
        })

        expect(residency.pinCount('viewport')).toBe(2)
        expect(residency.residentBytes).toBe(2)
        expect(residency.readRange(0, 2)).toEqual([message('zero'), message('one')])
        expect(residency.readRange(2, 1)).toBeNull()

        pinA.release()
        expect(residency.residentBytes).toBe(1)
        expect(residency.readRange(0, 1)).toEqual([message('zero')])
        expect(residency.readRange(1, 1)).toBeNull()
        pinB.release()
        pinB.release()
        expect(residency.pinCount('viewport')).toBe(0)
    })

    it('retains strict dirty replacements after save failure and evicts only after acknowledgement', () => {
        const residency = createResidency({ totalMessages: 2, maxResidentBytes: 0 })

        residency.recordReplaceRange({
            start: 1,
            deleteCount: 1,
            messages: [message('dirty')],
            sessionVersion: 1,
        })
        const failedSave = residency.beginPersistence(1)

        expect(residency.pendingMutations).toEqual([{
            start: 1,
            deleteCount: 1,
            messages: [message('dirty')],
            sessionVersion: 1,
        }])
        expect(residency.pinCount('dirty')).toBe(1)
        expect(residency.pinCount('pending-save')).toBe(1)
        expect(residency.readRange(1, 1)).toEqual([message('dirty')])

        failedSave.release()
        expect(residency.pinCount('pending-save')).toBe(0)
        expect(residency.pinCount('dirty')).toBe(1)
        expect(residency.readRange(1, 1)).toEqual([message('dirty')])

        const successfulSave = residency.beginPersistence(1)
        successfulSave.acknowledge()
        successfulSave.acknowledge()

        expect(residency.persistedVersion).toBe(1)
        expect(residency.pendingMutations).toEqual([])
        expect(residency.pinCount('dirty')).toBe(0)
        expect(residency.pinCount('pending-save')).toBe(0)
        expect(residency.readRange(1, 1)).toBeNull()
    })

    it('accounts for superseded dirty payloads until their persisted version is acknowledged', () => {
        const residency = createResidency({ totalMessages: 1, maxResidentBytes: 0 })

        residency.recordReplaceRange({
            start: 0,
            deleteCount: 1,
            messages: [message('first-dirty-payload')],
            sessionVersion: 1,
        })
        residency.recordReplaceRange({
            start: 0,
            deleteCount: 1,
            messages: [],
            sessionVersion: 2,
        })

        expect(residency.totalMessages).toBe(0)
        expect(residency.residentIntervals).toEqual([])
        expect(residency.residentBytes).toBe(1)

        residency.acknowledgePersisted(1)

        expect(residency.residentBytes).toBe(0)
        expect(residency.pendingMutations).toEqual([{
            start: 0,
            deleteCount: 1,
            messages: [],
            sessionVersion: 2,
        }])
    })

    it('keeps a streaming overlay resident outside the byte budget and releases it explicitly', () => {
        const residency = createResidency({ totalMessages: 1, maxResidentBytes: 0 })

        residency.setStreamingOverlay(0, message('streaming'), 1)

        expect(residency.pinCount('streaming')).toBe(1)
        expect(residency.readRange(0, 1)).toEqual([message('streaming')])
        expect(residency.residentBytes).toBe(1)

        residency.clearStreamingOverlay(1)

        expect(residency.pinCount('streaming')).toBe(0)
        expect(residency.readRange(0, 1)).toBeNull()
        expect(residency.residentBytes).toBe(0)
    })

    it('applies strict structural replacements without silently clamping or losing shifted entries', () => {
        const residency = createResidency({ totalMessages: 4 })
        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 4,
            messages: [message('zero'), message('one'), message('two'), message('three')],
        })

        expect(() => residency.recordReplaceRange({
            start: 4,
            deleteCount: 1,
            messages: [],
            sessionVersion: 1,
        })).toThrow('deleteCount exceeds')
        expect(residency.totalMessages).toBe(4)

        residency.recordReplaceRange({
            start: 1,
            deleteCount: 2,
            messages: [message('replacement')],
            sessionVersion: 1,
        })

        expect(residency.totalMessages).toBe(3)
        expect(residency.readRange(0, 3)).toEqual([
            message('zero'),
            message('replacement'),
            message('three'),
        ])
        expect(residency.pendingMutations).toEqual([{
            start: 1,
            deleteCount: 2,
            messages: [message('replacement')],
            sessionVersion: 1,
        }])
        expect(() => residency.acknowledgePersisted(2)).toThrow('current session version')
        expect(() => residency.recordReplaceRange({
            start: 0,
            deleteCount: 0,
            messages: [],
            sessionVersion: 3,
        })).toThrow('next session version')
    })
})
