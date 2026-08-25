import { describe, expect, it, vi } from 'vitest'

import type { PersistentRevisionLease } from './persistentDataStore'
import {
    exportNativePersistentRisuSave,
    nativePersistentRevisionLease,
    withPinnedNativePersistentRisuSaveFile,
} from './nativePersistentExport'

function harness() {
    const events: string[] = []
    const sourceBytes = new Uint8Array([1, 2, 3, 4, 5, 6, 7])
    const destinationBytes: number[] = []
    let sourceOffset = 0
    let revision = 4
    const flushPendingData = vi.fn(async () => {
        events.push('flush')
        revision = 5
    })
    const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
        events.push(`${command}:${JSON.stringify(args ?? {})}`)
        if (command === 'pds_acquire_revision') return { lease: 'snapshot-5-test' }
        if (command === 'pds_export_risu_save') {
            return {
                path: 'C:\\app\\persistent\\exports\\risusave-test.risudat',
                bytes: sourceBytes.byteLength,
            }
        }
        return undefined
    })
    const source = {
        read: vi.fn(async (buffer: Uint8Array) => {
            events.push('source:read')
            if (sourceOffset === sourceBytes.byteLength) return null
            const length = Math.min(3, sourceBytes.byteLength - sourceOffset)
            buffer.set(sourceBytes.subarray(sourceOffset, sourceOffset + length))
            sourceOffset += length
            return length
        }),
        write: vi.fn(async () => 0),
        close: vi.fn(async () => {
            events.push('source:close')
        }),
    }
    const destination = {
        read: vi.fn(async () => null),
        write: vi.fn(async (bytes: Uint8Array) => {
            events.push(`destination:write:${Array.from(bytes).join(',')}`)
            const length = Math.min(2, bytes.byteLength)
            destinationBytes.push(...bytes.subarray(0, length))
            return length
        }),
        close: vi.fn(async () => {
            events.push('destination:close')
        }),
    }
    const open = vi.fn(async (path: string, options: Record<string, boolean>) => {
        events.push(`open:${path}:${JSON.stringify(options)}`)
        return path.startsWith('content://') ? destination : source
    })
    return {
        destination,
        destinationBytes,
        events,
        source,
        runtime: {
            get revision() { return revision },
            flushPendingData,
        },
        dependencies: {
            isTauri: () => true,
            invoke,
            open,
        },
    }
}

describe('native persistent RisuSave export', () => {
    it('flushes, pins, exports, copies, cleans and releases in order', async () => {
        const { events, runtime, dependencies } = harness()

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            { omitAccount: true },
            dependencies,
        )).resolves.toEqual({ revision: 5, bytes: 7 })

        expect(dependencies.open).toHaveBeenNthCalledWith(
            1,
            'C:\\app\\persistent\\exports\\risusave-test.risudat',
            { read: true },
        )
        expect(dependencies.open).toHaveBeenNthCalledWith(
            2,
            'content://documents/export.risudat',
            { write: true, create: true, truncate: true },
        )
        expect(events.indexOf('destination:close')).toBeLessThan(
            events.indexOf('source:close'),
        )
        expect(events.indexOf('source:close')).toBeLessThan(
            events.findIndex((event) => event.startsWith('pds_export_risu_save_cleanup')),
        )
        expect(events.at(-1)).toBe('pds_release_revision:{"lease":"snapshot-5-test"}')
    })

    it('retries a transient native lease release within the successful export', async () => {
        const { events, runtime, dependencies } = harness()
        const invoke = dependencies.invoke.getMockImplementation()!
        let releaseAttempts = 0
        dependencies.invoke.mockImplementation(async (command, args) => {
            if (command !== 'pds_release_revision') return invoke(command, args)
            events.push(`${command}:${JSON.stringify(args ?? {})}`)
            releaseAttempts += 1
            if (releaseAttempts === 1) throw new Error('release unavailable')
            return undefined
        })

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).resolves.toEqual({ revision: 5, bytes: 7 })

        expect(releaseAttempts).toBe(2)
        expect(events.slice(-2)).toEqual([
            'pds_release_revision:{"lease":"snapshot-5-test"}',
            'pds_release_revision:{"lease":"snapshot-5-test"}',
        ])
    })

    it('preserves a copy failure when both native lease release attempts fail', async () => {
        const { destination, events, runtime, dependencies } = harness()
        const primaryError = new Error('destination write failed')
        destination.write.mockRejectedValueOnce(primaryError)
        const invoke = dependencies.invoke.getMockImplementation()!
        let releaseAttempts = 0
        dependencies.invoke.mockImplementation(async (command, args) => {
            if (command !== 'pds_release_revision') return invoke(command, args)
            events.push(`${command}:${JSON.stringify(args ?? {})}`)
            releaseAttempts += 1
            throw new Error('release unavailable')
        })

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).rejects.toBe(primaryError)

        expect(releaseAttempts).toBe(2)
        expect(events.slice(-2)).toEqual([
            'pds_release_revision:{"lease":"snapshot-5-test"}',
            'pds_release_revision:{"lease":"snapshot-5-test"}',
        ])
    })

    it('rejects non-Tauri callers before flushing', async () => {
        const { runtime, dependencies } = harness()
        dependencies.isTauri = () => false

        await expect(exportNativePersistentRisuSave(
            runtime,
            'export.risudat',
            {},
            dependencies,
        )).rejects.toThrow('requires Tauri')

        expect(runtime.flushPendingData).not.toHaveBeenCalled()
        expect(dependencies.invoke).not.toHaveBeenCalled()
    })

    it('handles partial writes while copying to a content URI', async () => {
        const { destinationBytes, runtime, source, dependencies } = harness()

        await exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )

        expect(destinationBytes).toEqual([1, 2, 3, 4, 5, 6, 7])
        expect(source.read.mock.calls[0][0]).toHaveLength(1024 * 1024)
    })

    it('preserves a write failure while closing both files, cleaning and releasing', async () => {
        const { destination, events, runtime, source, dependencies } = harness()
        const writeError = new Error('destination write failed')
        destination.write.mockRejectedValueOnce(writeError)

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).rejects.toBe(writeError)

        expect(destination.close).toHaveBeenCalledOnce()
        expect(source.close).toHaveBeenCalledOnce()
        expect(events.slice(-2)).toEqual([
            'pds_export_risu_save_cleanup:{"path":"C:\\\\app\\\\persistent\\\\exports\\\\risusave-test.risudat"}',
            'pds_release_revision:{"lease":"snapshot-5-test"}',
        ])
    })

    it('closes the source and cleans the export when the content URI cannot open', async () => {
        const { events, runtime, source, dependencies } = harness()
        const openError = new Error('destination open failed')
        dependencies.open
            .mockResolvedValueOnce(source)
            .mockRejectedValueOnce(openError)

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).rejects.toBe(openError)

        expect(source.close).toHaveBeenCalledOnce()
        expect(events.slice(-2)).toEqual([
            'pds_export_risu_save_cleanup:{"path":"C:\\\\app\\\\persistent\\\\exports\\\\risusave-test.risudat"}',
            'pds_release_revision:{"lease":"snapshot-5-test"}',
        ])
    })

    it('preserves a source read failure over close errors and still cleans', async () => {
        const { destination, events, runtime, source, dependencies } = harness()
        const readError = new Error('source read failed')
        source.read.mockRejectedValueOnce(readError)
        destination.close.mockRejectedValueOnce(new Error('destination close failed'))

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).rejects.toBe(readError)

        expect(destination.close).toHaveBeenCalledOnce()
        expect(source.close).toHaveBeenCalledOnce()
        expect(events.slice(-2)).toEqual([
            'pds_export_risu_save_cleanup:{"path":"C:\\\\app\\\\persistent\\\\exports\\\\risusave-test.risudat"}',
            'pds_release_revision:{"lease":"snapshot-5-test"}',
        ])
    })

    it('propagates a destination close failure after closing the source and cleaning', async () => {
        const { destination, events, runtime, source, dependencies } = harness()
        const closeError = new Error('destination close failed')
        destination.close.mockRejectedValueOnce(closeError)

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).rejects.toBe(closeError)

        expect(source.close).toHaveBeenCalledOnce()
        expect(events.slice(-2)).toEqual([
            'pds_export_risu_save_cleanup:{"path":"C:\\\\app\\\\persistent\\\\exports\\\\risusave-test.risudat"}',
            'pds_release_revision:{"lease":"snapshot-5-test"}',
        ])
    })

    it('releases the lease when native export fails before producing a file', async () => {
        const { events, runtime, dependencies } = harness()
        const exportError = new Error('native export failed')
        dependencies.invoke.mockImplementation(async (command, args) => {
            events.push(`${command}:${JSON.stringify(args ?? {})}`)
            if (command === 'pds_acquire_revision') return { lease: 'snapshot-5-test' }
            if (command === 'pds_export_risu_save') throw exportError
            return undefined
        })

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).rejects.toBe(exportError)

        expect(events).not.toContainEqual(expect.stringContaining('cleanup'))
        expect(events.at(-1)).toBe('pds_release_revision:{"lease":"snapshot-5-test"}')
    })

    it('still releases the lease when temporary cleanup fails', async () => {
        const { events, runtime, dependencies } = harness()
        const cleanupError = new Error('cleanup failed')
        dependencies.invoke.mockImplementation(async (command, args) => {
            events.push(`${command}:${JSON.stringify(args ?? {})}`)
            if (command === 'pds_acquire_revision') return { lease: 'snapshot-5-test' }
            if (command === 'pds_export_risu_save') {
                return { path: 'C:\\app\\persistent\\exports\\risusave-test.risudat', bytes: 7 }
            }
            if (command === 'pds_export_risu_save_cleanup') throw cleanupError
            return undefined
        })

        await expect(exportNativePersistentRisuSave(
            runtime,
            'content://documents/export.risudat',
            {},
            dependencies,
        )).rejects.toBe(cleanupError)

        expect(events.at(-1)).toBe('pds_release_revision:{"lease":"snapshot-5-test"}')
    })

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
})
