import { describe, expect, it } from 'vitest'
import { SegmentedConversationResidency } from './segmentedConversationResidency'
import type { Message } from './database.svelte'

function message(data: string): Message { return { role: 'char', data } }
function createResidency(options: { totalMessages?: number; maxResidentBytes?: number } = {}) {
    return new SegmentedConversationResidency({ revision: 7, totalMessages: options.totalMessages ?? 6,
        maxResidentBytes: options.maxResidentBytes ?? 0, measureMessage: () => 1 })
}

describe('production conversation mutation residency', () => {
    it('keeps a counted range pin through acknowledgement and evicts after its last release', () => {
        const residency = createResidency({ totalMessages: 2 })
        const first = residency.pinRange(1, 2, 'viewport')
        const second = residency.pinRange(1, 2, 'viewport')
        residency.recordReplaceRange({ start: 1, deleteCount: 1, messages: [message('edited')], sessionVersion: 1 })
        residency.acknowledgePersisted(1, 8)
        expect(residency.readRange(1, 1)).toEqual([message('edited')])
        first.release()
        first.release()
        expect(residency.pinCount('viewport')).toBe(1)
        second.release()
        expect(residency.residentBytes).toBe(0)
    })

    it('preserves strict structural mutations and rebases later edits across acknowledgement', () => {
        const residency = createResidency({ totalMessages: 3 })
        const pin = residency.pinRange(1, 2, 'viewport')
        residency.recordReplaceRange({ start: 1, deleteCount: 1, messages: [message('A'), message('B')], sessionVersion: 1 })
        residency.recordReplaceRange({ start: 2, deleteCount: 1, messages: [message('C')], sessionVersion: 2 })
        expect(residency.totalMessages).toBe(4)
        expect(residency.readRange(1, 2)).toEqual([message('A'), message('C')])
        // The edit inside the inserted range folds into it, releasing the superseded payload.
        expect(residency.pinCount('dirty')).toBe(1)
        expect(residency.residentBytes).toBe(2)
        residency.acknowledgePersisted(1, 8)
        expect(residency.totalMessages).toBe(4)
        expect(residency.persistentTotalMessages).toBe(4)
        expect(residency.pendingMutations).toEqual([
            { start: 1, deleteCount: 2, messages: [message('A'), message('C')], sessionVersion: 2 },
        ])
        expect(residency.readRange(1, 2)).toEqual([message('A'), message('C')])
        expect(() => residency.recordReplaceRange({ start: 5, deleteCount: 0, messages: [], sessionVersion: 3 })).toThrow()
        expect(residency.sessionVersion).toBe(2)
        residency.acknowledgePersisted(2, 9)
        expect(residency.persistentTotalMessages).toBe(4)
        expect(residency.pendingMutations).toEqual([])
        expect(residency.residentIntervals).toEqual([
            { startIndex: 1, endIndex: 3, byteSize: 2, messages: [message('A'), message('C')] },
        ])
        pin.release()
        expect(residency.residentBytes).toBe(0)
    })

    it('records count-preserving edits outside the newest unsaved range separately', () => {
        const residency = createResidency({ totalMessages: 4 })
        residency.recordReplaceRange({ start: 2, deleteCount: 1, messages: [message('A')], sessionVersion: 1 })
        residency.recordReplaceRange({ start: 1, deleteCount: 1, messages: [message('B')], sessionVersion: 2 })
        residency.recordReplaceRange({ start: 2, deleteCount: 1, messages: [message('C')], sessionVersion: 3 })
        expect(residency.pendingMutations).toEqual([
            { start: 2, deleteCount: 1, messages: [message('A')], sessionVersion: 1 },
            { start: 1, deleteCount: 1, messages: [message('B')], sessionVersion: 2 },
            { start: 2, deleteCount: 1, messages: [message('C')], sessionVersion: 3 },
        ])
        residency.acknowledgePersisted(2, 8)
        expect(residency.persistentTotalMessages).toBe(4)
        expect(residency.readRange(1, 2)).toEqual([message('B'), message('C')])
    })

    it('preserves mutation and pin state when replacement measurement fails', () => {
        const residency = new SegmentedConversationResidency({ revision: 7, totalMessages: 2,
            maxResidentBytes: 0, measureMessage: () => -1 })
        const pin = residency.pinRange(1, 2, 'viewport')
        expect(() => residency.recordReplaceRange({ start: 0, deleteCount: 0, messages: [message('invalid')], sessionVersion: 1 }))
            .toThrow('nonnegative safe integer')
        expect(residency.totalMessages).toBe(2)
        expect(residency.sessionVersion).toBe(0)
        expect(residency.pendingMutations).toEqual([])
        expect(residency.pinCount('viewport')).toBe(1)
        pin.release()
    })

    it('discards retained dirty and pending-save payloads when the complete owner takes over', () => {
        const residency = createResidency({ totalMessages: 1 })
        residency.recordReplaceRange({ start: 0, deleteCount: 1, messages: [message('dirty')], sessionVersion: 1 })
        const pending = residency.beginPersistence(1)
        residency.discardResidentState()
        expect(residency.residentBytes).toBe(0)
        expect(residency.pendingMutations).toEqual([])
        expect(residency.pinCount('pending-save')).toBe(0)
        pending.release()
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
        successfulSave.acknowledge(8)
        successfulSave.acknowledge(8)

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

        residency.acknowledgePersisted(1, 8)

        expect(residency.residentBytes).toBe(0)
        expect(residency.pendingMutations).toEqual([{
            start: 0,
            deleteCount: 1,
            messages: [],
            sessionVersion: 2,
        }])
    })

    it('releases stale out-of-order persistence attempts without rewinding state', () => {
        const residency = createResidency({ totalMessages: 1, maxResidentBytes: 0 })
        residency.recordReplaceRange({
            start: 0,
            deleteCount: 1,
            messages: [message('v1')],
            sessionVersion: 1,
        })
        const firstSave = residency.beginPersistence(1)
        residency.recordReplaceRange({
            start: 0,
            deleteCount: 1,
            messages: [message('v2')],
            sessionVersion: 2,
        })
        const secondSave = residency.beginPersistence(2)

        expect(residency.pinCount('dirty')).toBe(1)
        expect(residency.pinCount('pending-save')).toBe(2)
        expect(residency.residentBytes).toBe(1)

        secondSave.acknowledge(9)
        expect(() => firstSave.acknowledge(8)).not.toThrow()

        expect(residency.revision).toBe(9)
        expect(residency.persistedVersion).toBe(2)
        expect(residency.pinCount('dirty')).toBe(0)
        expect(residency.pinCount('pending-save')).toBe(0)
    })

})
