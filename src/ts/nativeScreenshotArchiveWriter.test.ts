import { describe, expect, it, vi } from 'vitest'
import {
    SCREENSHOT_OUTPUT_CHUNK_BYTES,
    createNativeScreenshotArchiveWriter,
} from './nativeScreenshotArchiveWriter'

function harness(destination: string | null = 'C:\\chosen\\chat.zip') {
    const calls: Array<[string, Record<string, unknown> | undefined]> = []
    const dependencies = {
        selectDestination: vi.fn(async () => destination),
        warn: vi.fn(),
        invoke: vi.fn(async (
            command: string,
            args?: Record<string, unknown>,
        ): Promise<unknown> => {
            calls.push([command, args])
            if (command === 'native_file_job_screenshot_output_start') {
                return { jobId: 'screenshot-1' }
            }
            if (command === 'native_file_job_screenshot_output_publish') {
                return { bytes: 7 }
            }
            if (command === 'native_file_job_screenshot_output_cancel') return 'requested'
            if (command === 'native_file_job_screenshot_output_append') return undefined
            throw new Error(`Unexpected command: ${command}`)
        }),
    }
    return { calls, dependencies }
}

describe('native screenshot archive writer', () => {
    it('uses the existing save dialog destination and keeps IPC chunks at or below 64 KiB', async () => {
        const { calls, dependencies } = harness()
        const writer = await createNativeScreenshotArchiveWriter('chat.zip', dependencies)
        const bytes = new Uint8Array(SCREENSHOT_OUTPUT_CHUNK_BYTES * 2 + 7)
        bytes[bytes.length - 1] = 9

        await writer.write(bytes)
        await writer.close()

        expect(dependencies.selectDestination).toHaveBeenCalledWith('chat.zip')
        expect(calls[0]).toEqual([
            'native_file_job_screenshot_output_start',
            { destination: 'C:\\chosen\\chat.zip' },
        ])
        const appends = calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_append')
        expect(appends).toHaveLength(3)
        expect(appends.map(([, args]) => (args?.chunk as number[]).length)).toEqual([
            SCREENSHOT_OUTPUT_CHUNK_BYTES,
            SCREENSHOT_OUTPUT_CHUNK_BYTES,
            7,
        ])
        expect((appends[2][1]?.chunk as number[]).at(-1)).toBe(9)
        expect(calls.at(-1)).toEqual([
            'native_file_job_screenshot_output_publish',
            { jobId: 'screenshot-1' },
        ])
        await expect(writer.write(Uint8Array.of(1))).rejects.toThrow('finalized')
    })

    it('fails with AbortError before creating a native job when the dialog is cancelled', async () => {
        const { calls, dependencies } = harness(null)

        await expect(createNativeScreenshotArchiveWriter('chat.zip', dependencies))
            .rejects.toMatchObject({ name: 'AbortError' })
        expect(calls).toEqual([])
    })

    it('cancels an open job once and rejects later writes', async () => {
        const { calls, dependencies } = harness()
        const writer = await createNativeScreenshotArchiveWriter('chat.zip', dependencies)

        await writer.abort()
        await writer.abort()

        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_cancel')).toEqual([[
            'native_file_job_screenshot_output_cancel',
            { jobId: 'screenshot-1' },
        ]])
        await expect(writer.write(Uint8Array.of(1))).rejects.toThrow('aborted')
        await expect(writer.close()).rejects.toThrow('aborted')
    })

    it('keeps the job abortable when destination publication fails', async () => {
        const { calls, dependencies } = harness()
        dependencies.invoke.mockImplementation(async (command, args) => {
            calls.push([command, args])
            if (command === 'native_file_job_screenshot_output_start') {
                return { jobId: 'screenshot-1' }
            }
            if (command === 'native_file_job_screenshot_output_publish') {
                throw new Error('disk full')
            }
            if (command === 'native_file_job_screenshot_output_cancel') return 'requested'
            return undefined
        })
        const writer = await createNativeScreenshotArchiveWriter('chat.zip', dependencies)

        await expect(writer.close()).rejects.toThrow('disk full')
        await writer.abort()

        expect(calls.at(-1)).toEqual([
            'native_file_job_screenshot_output_cancel',
            { jobId: 'screenshot-1' },
        ])
    })

    it('reconciles a too-late cancellation as a completed publication', async () => {
        let finishPublication!: () => void
        const publication = new Promise<void>((resolve) => {
            finishPublication = resolve
        })
        const { calls, dependencies } = harness()
        dependencies.invoke.mockImplementation(async (command, args) => {
            calls.push([command, args])
            if (command === 'native_file_job_screenshot_output_start') {
                return { jobId: 'screenshot-1' }
            }
            if (command === 'native_file_job_screenshot_output_publish') return publication
            if (command === 'native_file_job_screenshot_output_cancel') return 'tooLate'
            return undefined
        })
        const writer = await createNativeScreenshotArchiveWriter('chat.zip', dependencies)

        const close = writer.close()
        const abort = writer.abort()
        finishPublication()

        await expect(close).resolves.toBeUndefined()
        await expect(abort).resolves.toBe(false)
        await expect(writer.close()).resolves.toBeUndefined()
        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_cancel')).toHaveLength(1)
    })

    it('surfaces a cleanup warning once without failing or retrying publication', async () => {
        const { calls, dependencies } = harness()
        dependencies.invoke.mockImplementation(async (command, args) => {
            calls.push([command, args])
            if (command === 'native_file_job_screenshot_output_start') {
                return { jobId: 'screenshot-1' }
            }
            if (command === 'native_file_job_screenshot_output_publish') {
                return {
                    bytes: 7,
                    warningCodes: ['cleanup-failed', 'cleanup-failed'],
                }
            }
            return undefined
        })
        const writer = await createNativeScreenshotArchiveWriter('chat.zip', dependencies)

        await expect(writer.close()).resolves.toBeUndefined()

        expect(dependencies.warn).toHaveBeenCalledOnce()
        expect(dependencies.warn).toHaveBeenCalledWith('cleanup-failed')
        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_publish')).toHaveLength(1)
    })
})
