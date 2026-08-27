import { describe, expect, it } from 'vitest'

import type { Message } from './database.svelte'
import { DisposableConversationSnapshot } from './persistentConversationSegments'
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
    it('discards every retained payload when the complete owner takes over', () => {
        const residency = createResidency({ maxResidentBytes: 1 })
        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 6,
            messages: [message('zero')],
        })
        const rangePin = residency.pinRange(0, 1, 'viewport')
        residency.recordReplaceRange({
            start: 6,
            deleteCount: 0,
            messages: [message('dirty')],
            sessionVersion: 1,
        })
        const pendingSave = residency.beginPersistence(1)
        residency.setStreamingOverlay(0, message('streaming'), 1)

        residency.discardResidentState()

        expect(residency.residentBytes).toBe(0)
        expect(residency.residentIntervals).toEqual([])
        expect(residency.pendingMutations).toEqual([])
        expect(residency.pinCount('viewport')).toBe(0)
        expect(residency.pinCount('dirty')).toBe(0)
        expect(residency.pinCount('pending-save')).toBe(0)
        expect(residency.pinCount('streaming')).toBe(0)
        expect(() => rangePin.release()).not.toThrow()
        expect(() => pendingSave.release()).not.toThrow()
    })

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
        expect(residency.missingPersistentRanges(0, 6)).toEqual([{
            revision: 7,
            currentStartIndex: 4,
            currentEndIndex: 6,
            persistentStartIndex: 4,
            persistentEndIndex: 6,
        }])
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
        successfulSave.acknowledge(8)
        successfulSave.acknowledge(8)

        expect(residency.persistedVersion).toBe(1)
        expect(residency.pendingMutations).toEqual([])
        expect(residency.pinCount('dirty')).toBe(0)
        expect(residency.pinCount('pending-save')).toBe(0)
        expect(residency.readRange(1, 1)).toBeNull()
    })

    it('blocks every eviction while dirty, pending-save, or streaming state is active', () => {
        const residency = createResidency({ totalMessages: 3, maxResidentBytes: 1 })
        const initialPin = residency.pinRange(0, 3, 'viewport')
        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 3,
            messages: [message('zero'), message('one'), message('two')],
        })
        residency.recordReplaceRange({
            start: 2,
            deleteCount: 1,
            messages: [message('dirty')],
            sessionVersion: 1,
        })

        initialPin.release()
        expect(residency.readRange(0, 3)).toEqual([
            message('zero'),
            message('one'),
            message('dirty'),
        ])

        const staleSave = residency.beginPersistence(1)
        const successfulSave = residency.beginPersistence(1)
        successfulSave.acknowledge(8)
        expect(residency.pinCount('dirty')).toBe(0)
        expect(residency.pinCount('pending-save')).toBe(1)
        expect(residency.readRange(0, 3)).toEqual([
            message('zero'),
            message('one'),
            message('dirty'),
        ])

        residency.setStreamingOverlay(2, message('streaming'), 1)
        staleSave.release()
        expect(residency.pinCount('pending-save')).toBe(0)
        expect(residency.pinCount('streaming')).toBe(1)
        expect(residency.readRange(0, 3)).toEqual([
            message('zero'),
            message('one'),
            message('streaming'),
        ])

        residency.clearStreamingOverlay(1)
        expect(residency.residentBytes).toBeLessThanOrEqual(1)
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
        expect(() => residency.acknowledgePersisted(2, 8)).toThrow('current session version')
        expect(() => residency.recordReplaceRange({
            start: 0,
            deleteCount: 0,
            messages: [],
            sessionVersion: 3,
        })).toThrow('next session version')
    })

    it('projects current gaps onto the pinned base revision after insertions and deletions', () => {
        const residency = createResidency({ totalMessages: 4 })
        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 4,
            messages: [message('A')],
        })

        residency.recordReplaceRange({
            start: 1,
            deleteCount: 0,
            messages: [message('X')],
            sessionVersion: 1,
        })

        expect(residency.missingPersistentRanges(2, 3)).toEqual([{
            revision: 7,
            currentStartIndex: 2,
            currentEndIndex: 5,
            persistentStartIndex: 1,
            persistentEndIndex: 4,
        }])

        residency.storeRange({
            revision: 7,
            startIndex: 1,
            totalMessages: 4,
            messages: [message('B'), message('C'), message('D')],
        })
        expect(residency.readRange(0, 5)).toEqual([
            message('A'),
            message('X'),
            message('B'),
            message('C'),
            message('D'),
        ])

        residency.recordReplaceRange({
            start: 2,
            deleteCount: 1,
            messages: [],
            sessionVersion: 2,
        })
        expect(residency.readRange(0, 4)).toEqual([
            message('A'),
            message('X'),
            message('C'),
            message('D'),
        ])
        expect(residency.missingPersistentRanges(0, 4)).toEqual([])

        const deletionResidency = createResidency({ totalMessages: 4 })
        deletionResidency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 4,
            messages: [message('A')],
        })
        deletionResidency.recordReplaceRange({
            start: 1,
            deleteCount: 1,
            messages: [],
            sessionVersion: 1,
        })
        expect(deletionResidency.missingPersistentRanges(1, 2)).toEqual([{
            revision: 7,
            currentStartIndex: 1,
            currentEndIndex: 3,
            persistentStartIndex: 2,
            persistentEndIndex: 4,
        }])
        deletionResidency.storeRange({
            revision: 7,
            startIndex: 2,
            totalMessages: 4,
            messages: [message('C'), message('D')],
        })
        expect(deletionResidency.readRange(0, 3)).toEqual([
            message('A'),
            message('C'),
            message('D'),
        ])
    })

    it('advances the pinned revision after persistence and rebases newer dirty mutations', () => {
        const residency = createResidency({ totalMessages: 4 })
        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 4,
            messages: [message('A')],
        })
        residency.recordReplaceRange({
            start: 1,
            deleteCount: 0,
            messages: [message('X')],
            sessionVersion: 1,
        })
        const firstSave = residency.beginPersistence(1)
        residency.recordReplaceRange({
            start: 3,
            deleteCount: 1,
            messages: [],
            sessionVersion: 2,
        })
        const pin = residency.pinRange(2, 4, 'viewport')

        firstSave.acknowledge(8)

        expect(residency.revision).toBe(8)
        expect(residency.persistentTotalMessages).toBe(5)
        expect(residency.totalMessages).toBe(4)
        expect(residency.persistedVersion).toBe(1)
        expect(residency.pinCount('viewport')).toBe(1)
        expect(residency.pendingMutations).toEqual([{
            start: 3,
            deleteCount: 1,
            messages: [],
            sessionVersion: 2,
        }])
        expect(residency.missingPersistentRanges(2, 2)).toEqual([
            {
                revision: 8,
                currentStartIndex: 2,
                currentEndIndex: 3,
                persistentStartIndex: 2,
                persistentEndIndex: 3,
            },
            {
                revision: 8,
                currentStartIndex: 3,
                currentEndIndex: 4,
                persistentStartIndex: 4,
                persistentEndIndex: 5,
            },
        ])

        residency.storeRange({
            revision: 8,
            startIndex: 2,
            totalMessages: 5,
            messages: [message('B')],
        })
        residency.storeRange({
            revision: 8,
            startIndex: 4,
            totalMessages: 5,
            messages: [message('D')],
        })
        expect(residency.readRange(0, 4)).toEqual([
            message('A'),
            message('X'),
            message('B'),
            message('D'),
        ])
        pin.release()
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

        expect(residency.pinCount('dirty')).toBe(2)
        expect(residency.pinCount('pending-save')).toBe(2)
        expect(residency.residentBytes).toBe(2)

        secondSave.acknowledge(9)
        expect(() => firstSave.acknowledge(8)).not.toThrow()

        expect(residency.revision).toBe(9)
        expect(residency.persistedVersion).toBe(2)
        expect(residency.pinCount('dirty')).toBe(0)
        expect(residency.pinCount('pending-save')).toBe(0)
    })

    it('leaves residency and pin coordinates unchanged when replacement measurement fails', () => {
        const residency = new SegmentedConversationResidency({
            revision: 7,
            totalMessages: 2,
            maxResidentBytes: 0,
            measureMessage: (item) => item.data === 'invalid' ? -1 : 1,
        })
        const pin = residency.pinRange(1, 2, 'viewport')
        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 2,
            messages: [message('A'), message('B')],
        })

        expect(() => residency.recordReplaceRange({
            start: 0,
            deleteCount: 0,
            messages: [message('invalid')],
            sessionVersion: 1,
        })).toThrow('nonnegative safe integer')

        expect(residency.totalMessages).toBe(2)
        expect(residency.sessionVersion).toBe(0)
        expect(residency.pendingMutations).toEqual([])
        expect(residency.pinCount('viewport')).toBe(1)
        expect(residency.readRange(1, 1)).toEqual([message('B')])

        pin.release()
    })

    it('leaves cached data and an existing overlay unchanged when overlay measurement fails', () => {
        const residency = new SegmentedConversationResidency({
            revision: 7,
            totalMessages: 2,
            maxResidentBytes: 64,
            measureMessage: (item) => item.data === 'invalid' ? -1 : 1,
        })
        residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 2,
            messages: [message('A'), message('B')],
        })
        residency.setStreamingOverlay(0, message('streaming'), 1)

        expect(() => residency.setStreamingOverlay(1, message('invalid'), 1)).toThrow(
            'nonnegative safe integer',
        )

        expect(residency.pinCount('streaming')).toBe(1)
        expect(residency.residentBytes).toBe(3)
        expect(residency.readRange(0, 2)).toEqual([
            message('streaming'),
            message('B'),
        ])
    })

    it('does not partially store a persistent range when measurement fails', () => {
        const residency = new SegmentedConversationResidency({
            revision: 7,
            totalMessages: 3,
            maxResidentBytes: 64,
            measureMessage: (item) => item.data === 'invalid' ? -1 : 1,
        })
        residency.storeRange({
            revision: 7,
            startIndex: 2,
            totalMessages: 3,
            messages: [message('C')],
        })

        expect(() => residency.storeRange({
            revision: 7,
            startIndex: 0,
            totalMessages: 3,
            messages: [message('A'), message('invalid')],
        })).toThrow('nonnegative safe integer')

        expect(residency.residentBytes).toBe(1)
        expect(residency.readRange(2, 1)).toEqual([message('C')])
        expect(residency.missingPersistentRanges(0, 2)).toEqual([{
            revision: 7,
            currentStartIndex: 0,
            currentEndIndex: 2,
            persistentStartIndex: 0,
            persistentEndIndex: 2,
        }])
    })

    it('restores the underlying dirty message after clearing a streaming overlay', () => {
        const residency = createResidency({ totalMessages: 1, maxResidentBytes: 0 })
        residency.recordReplaceRange({
            start: 0,
            deleteCount: 1,
            messages: [message('dirty')],
            sessionVersion: 1,
        })

        residency.setStreamingOverlay(0, message('streaming'), 1)
        expect(residency.residentBytes).toBe(2)
        expect(residency.readRange(0, 1)).toEqual([message('streaming')])

        residency.clearStreamingOverlay(1)
        expect(residency.residentBytes).toBe(1)
        expect(residency.readRange(0, 1)).toEqual([message('dirty')])
        expect(residency.missingPersistentRanges(0, 1)).toEqual([])
    })

    it('returns a 10,000-message compatibility snapshot to the resident budget after release', () => {
        const totalMessages = 10_000
        const residentBudget = 256
        const messages = Array.from({ length: totalMessages }, (_, index) => message(String(index)))
        const residency = createResidency({ totalMessages, maxResidentBytes: residentBudget })
        const viewportPin = residency.pinRange(
            totalMessages - residentBudget,
            totalMessages,
            'viewport',
        )
        const compatibilityPin = residency.pinRange(0, totalMessages, 'compatibility')
        const snapshot = new DisposableConversationSnapshot(structuredClone(messages))

        for (let startIndex = 0; startIndex < totalMessages; startIndex += 128) {
            residency.storeRange({
                revision: 7,
                startIndex,
                totalMessages,
                messages: messages.slice(startIndex, startIndex + 128),
            })
        }

        expect(snapshot.residentMessageCount).toBe(totalMessages)
        expect(residency.pinCount('compatibility')).toBe(1)
        expect(residency.residentBytes).toBe(totalMessages)

        snapshot.dispose()
        compatibilityPin.release()

        expect(snapshot.residentMessageCount).toBe(0)
        expect(residency.pinCount('compatibility')).toBe(0)
        expect(residency.residentBytes).toBe(residentBudget)
        expect(residency.readRange(totalMessages - residentBudget, residentBudget)).toEqual(
            messages.slice(totalMessages - residentBudget),
        )
        expect(residency.readRange(0, 1)).toBeNull()
        expect(residency.residentIntervals).toHaveLength(1)
        expect(residency.residentIntervals[0]).toMatchObject({
            startIndex: totalMessages - residentBudget,
            endIndex: totalMessages,
            byteSize: residentBudget,
        })

        viewportPin.release()
    }, 15_000)
})
