import { beforeEach, describe, expect, it, vi } from 'vitest'

const bridge = vi.hoisted(() => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: bridge.invoke }))

import { createIOSScreenshotArchiveWriter, SCREENSHOT_OUTPUT_CHUNK_BYTES } from './nativeScreenshotArchiveWriter'

const sourcePath = '/synthetic/RisuNest/native-file-jobs/screenshot-output/job/archive.zip.part'

function harness() {
    let bytes = 0
    const exportFile = vi.fn(async (): Promise<{ cancelled: boolean; bytes: number }> => ({ cancelled: false, bytes }))
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
        switch (command) {
            case 'native_file_job_screenshot_output_start': return { jobId: 'screenshot-1' }
            case 'native_file_job_screenshot_output_append': {
                bytes += (args!.chunk as number[]).length
                return
            }
            case 'native_file_job_screenshot_output_publish': return { bytes, sourcePath }
            case 'plugin:ios-native|export_file': return exportFile()
            case 'native_file_job_screenshot_output_release': return
            case 'native_file_job_screenshot_output_cancel': return 'requested'
            default: throw new Error(`Unexpected command: ${command}`)
        }
    })
    return { exportFile }
}

const calls = (command: string) => bridge.invoke.mock.calls.filter(([name]) => name === command)
beforeEach(() => { bridge.invoke.mockReset() })

describe('iOS screenshot archive publication', () => {
    it('streams bounded chunks and passes only the native source path to Files', async () => {
        harness()
        const writer = await createIOSScreenshotArchiveWriter('chat.zip')
        await writer.write(new Uint8Array(SCREENSHOT_OUTPUT_CHUNK_BYTES * 2 + 7))
        await writer.close()
        expect(calls('native_file_job_screenshot_output_start')[0][1]).toEqual({ destination: null })
        const chunks = calls('native_file_job_screenshot_output_append').map(([, args]) => args.chunk)
        expect(chunks.map(chunk => chunk.length)).toEqual([SCREENSHOT_OUTPUT_CHUNK_BYTES, SCREENSHOT_OUTPUT_CHUNK_BYTES, 7])
        expect(calls('plugin:ios-native|export_file')[0][1]).toEqual({
            sourcePath,
            suggestedName: 'chat.zip',
            requestId: expect.any(String),
        })
        expect(calls('native_file_job_screenshot_output_release')).toHaveLength(1)
        expect(await writer.abort()).toBe(false)
    })

    it('cleans the native spool after the Files picker is cancelled', async () => {
        const { exportFile } = harness()
        exportFile.mockResolvedValue({ cancelled: true, bytes: 0 })
        const writer = await createIOSScreenshotArchiveWriter('chat.zip')
        await expect(writer.close()).rejects.toMatchObject({ name: 'AbortError' })
        expect(calls('native_file_job_screenshot_output_release')).toHaveLength(1)
    })

    it('does not turn a failed Files export into success', async () => {
        const { exportFile } = harness()
        exportFile.mockRejectedValue(new Error('provider failed'))
        const writer = await createIOSScreenshotArchiveWriter('chat.zip')
        await expect(writer.close()).rejects.toThrow('provider failed')
        expect(calls('native_file_job_screenshot_output_release')).toHaveLength(1)
    })

    it('retains the source until an already-open picker finishes, preserving late success', async () => {
        const { exportFile } = harness()
        let finish!: (value: { cancelled: boolean; bytes: number }) => void
        exportFile.mockImplementation(() => new Promise(resolve => { finish = resolve }))
        const writer = await createIOSScreenshotArchiveWriter('chat.zip')
        await writer.write(Uint8Array.of(1, 2, 3))
        const closing = writer.close()
        await vi.waitFor(() => expect(exportFile).toHaveBeenCalledOnce())
        const aborting = writer.abort()
        expect(calls('native_file_job_screenshot_output_release')).toHaveLength(0)
        expect(calls('native_file_job_screenshot_output_cancel')).toHaveLength(0)
        finish({ cancelled: false, bytes: 3 })
        await expect(closing).resolves.toBeUndefined()
        expect(await aborting).toBe(false)
        expect(calls('native_file_job_screenshot_output_release')).toHaveLength(1)
    })

    it('reports a mismatched exported length rather than a successful publication', async () => {
        const { exportFile } = harness()
        exportFile.mockResolvedValue({ cancelled: false, bytes: 1 })
        const writer = await createIOSScreenshotArchiveWriter('chat.zip')
        await expect(writer.close()).rejects.toMatchObject({
            code: 'length-mismatch',
            warningCodes: ['partial-destination-may-remain'],
        })
        expect(calls('native_file_job_screenshot_output_release')).toHaveLength(1)
    })
})
