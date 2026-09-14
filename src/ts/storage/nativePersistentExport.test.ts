import { describe, expect, it, vi } from 'vitest'

import type { PersistentRevisionLease } from './persistentDataStore'
import {
    nativePersistentRevisionLease,
    withPinnedNativePersistentRisuSaveFile,
} from './nativePersistentExport'

function harness() {
    const events: string[] = []
    const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
        events.push(`${command}:${JSON.stringify(args ?? {})}`)
        if (command === 'pds_export_risu_save') {
            return {
                path: 'C:\\app\\persistent\\exports\\risusave-test.risudat',
                bytes: 7,
            }
        }
        return undefined
    })
    return {
        events,
        dependencies: {
            isTauri: () => true,
            invoke,
        },
    }
}

describe('native persistent RisuSave export', () => {
    it('exports from an existing pinned lease without acquiring or releasing another lease', async () => {
        const { events, dependencies } = harness()
        const lease = {
            revision: 5,
            [nativePersistentRevisionLease]: 'snapshot-5-existing',
        } as unknown as PersistentRevisionLease

        await expect(withPinnedNativePersistentRisuSaveFile(
            lease,
            { omitAccount: true },
            async (file) => {
                events.push(`read:${file.path}`)
                return file.bytes
            },
            dependencies,
        )).resolves.toBe(7)

        expect(events).toEqual([
            'pds_export_risu_save:{"lease":"snapshot-5-existing","omitAccount":true}',
            'read:C:\\app\\persistent\\exports\\risusave-test.risudat',
            'pds_export_risu_save_cleanup:{"path":"C:\\\\app\\\\persistent\\\\exports\\\\risusave-test.risudat"}',
        ])
    })

    it('cleans an existing pinned lease export when its consumer fails', async () => {
        const { events, dependencies } = harness()
        const readError = new Error('read cancelled')
        const lease = {
            revision: 5,
            [nativePersistentRevisionLease]: 'snapshot-5-existing',
        } as unknown as PersistentRevisionLease

        await expect(withPinnedNativePersistentRisuSaveFile(
            lease,
            {},
            async () => {
                throw readError
            },
            dependencies,
        )).rejects.toBe(readError)

        expect(events.at(-1)).toBe(
            'pds_export_risu_save_cleanup:{"path":"C:\\\\app\\\\persistent\\\\exports\\\\risusave-test.risudat"}',
        )
        expect(events.some((event) => event.startsWith('pds_acquire_revision'))).toBe(false)
        expect(events.some((event) => event.startsWith('pds_release_revision'))).toBe(false)
    })

    it('rejects callers without a native lease', async () => {
        const { dependencies } = harness()
        expect(() => withPinnedNativePersistentRisuSaveFile(
                { revision: 5 } as PersistentRevisionLease,
                {},
                async () => undefined,
                dependencies,
            ))
            .toThrow('does not support native export')
    })
})
