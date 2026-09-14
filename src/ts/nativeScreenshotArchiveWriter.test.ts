import { describe, expect, it, vi } from 'vitest'
import type { AndroidSafDestinationRequest } from './storage/androidSafBridge'
import { createStreamingScreenshotArchive } from './chatScreenshotArchive'
import {
    SCREENSHOT_OUTPUT_CHUNK_BYTES,
    createAndroidScreenshotArchiveWriter,
    createNativeScreenshotArchiveWriter,
    describeScreenshotPublicationError,
    listenRecoveredAndroidScreenshotPublications,
    recoverAndroidScreenshotPublication,
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

describe('Android screenshot archive writer', () => {
    function androidHarness() {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const dependencies = {
            warn: vi.fn(),
            acknowledgeAndroidSafExport: vi.fn(() => true),
            copyToAndroidSaf: vi.fn(async (_request: AndroidSafDestinationRequest) => ({
                requestId: 'saf-request-1',
                bytes: 7,
                warningCodes: ['android-saf-provider-not-atomic'],
            })),
            invoke: vi.fn(async (
                command: string,
                args?: Record<string, unknown>,
            ): Promise<unknown> => {
                calls.push([command, args])
                if (command === 'native_file_job_screenshot_output_start') {
                    return { jobId: '11111111-1111-4111-8111-111111111111' }
                }
                if (command === 'native_file_job_screenshot_output_publish') {
                    return {
                        bytes: 7,
                        sourcePath: '/data/user/0/io.github.rsyumi.risunest/native-file-jobs/screenshot-output/11111111-1111-4111-8111-111111111111/archive.zip.part',
                        warningCodes: [],
                    }
                }
                if (command === 'native_file_job_screenshot_output_cancel') return 'requested'
                if (command === 'native_file_job_screenshot_output_release') return undefined
                if (command === 'native_file_job_screenshot_output_append') return undefined
                throw new Error(`Unexpected command: ${command}`)
            }),
        }
        return { calls, dependencies }
    }

    it('streams to an owned native ZIP, publishes through SAF, and releases the spool', async () => {
        const { calls, dependencies } = androidHarness()
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)

        await writer.write(Uint8Array.of(1, 2, 3))
        await writer.close()

        expect(calls[0]).toEqual([
            'native_file_job_screenshot_output_start',
            { destination: null },
        ])
        expect(dependencies.copyToAndroidSaf).toHaveBeenCalledExactlyOnceWith({
            sourcePath: expect.stringContaining('archive.zip.part'),
            suggestedName: 'chat.zip',
            signal: expect.any(AbortSignal),
            deferAcknowledgement: true,
        })
        expect(calls.at(-1)).toEqual([
            'native_file_job_screenshot_output_release',
            { jobId: '11111111-1111-4111-8111-111111111111' },
        ])
        expect(dependencies.warn).toHaveBeenCalledWith('android-saf-provider-not-atomic')
        expect(dependencies.acknowledgeAndroidSafExport)
            .toHaveBeenCalledExactlyOnceWith('saf-request-1')
    })

    it('fails publication when the SAF copy length differs from the native handoff', async () => {
        const { calls, dependencies } = androidHarness()
        dependencies.copyToAndroidSaf.mockResolvedValueOnce({
            requestId: 'saf-request-1',
            bytes: 6,
            warningCodes: [],
        })
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)

        await expect(writer.close()).rejects.toMatchObject({
            code: 'length-mismatch',
            warningCodes: ['partial-destination-may-remain'],
        })

        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_release')).toHaveLength(1)
        expect(dependencies.acknowledgeAndroidSafExport)
            .toHaveBeenCalledExactlyOnceWith('saf-request-1')
        expect(dependencies.warn).not.toHaveBeenCalledWith('length-mismatch')
    })

    it('does not reconcile cancellation as success when a committed SAF copy mismatches', async () => {
        const { calls, dependencies } = androidHarness()
        type CopyResult = { requestId: string; bytes: number; warningCodes: string[] }
        let finishCopy!: (result: CopyResult) => void
        dependencies.copyToAndroidSaf.mockImplementationOnce(() =>
            new Promise<CopyResult>((resolve) => {
                finishCopy = (result) => resolve(result)
            }),
        )
        const controller = new AbortController()
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)
        const archive = createStreamingScreenshotArchive(writer)
        await archive.addPage(1, new Blob(['page'], { type: 'image/png' }))

        const close = archive.close(controller.signal)
        await vi.waitFor(() => expect(dependencies.copyToAndroidSaf).toHaveBeenCalledOnce())
        controller.abort()
        finishCopy({
            requestId: 'saf-request-1',
            bytes: 6,
            warningCodes: [],
        })

        await expect(close).rejects.toMatchObject({
            code: 'length-mismatch',
            warningCodes: ['partial-destination-may-remain'],
        })
        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_release')).toHaveLength(1)
        expect(dependencies.acknowledgeAndroidSafExport)
            .toHaveBeenCalledExactlyOnceWith('saf-request-1')
    })

    it('cancels SAF and releases the owned spool without reporting publication', async () => {
        const { calls, dependencies } = androidHarness()
        let sourceAvailable = true
        dependencies.invoke.mockImplementation(async (command, args) => {
            calls.push([command, args])
            if (command === 'native_file_job_screenshot_output_start') {
                return { jobId: '11111111-1111-4111-8111-111111111111' }
            }
            if (command === 'native_file_job_screenshot_output_publish') {
                return {
                    bytes: 7,
                    sourcePath: '/data/user/0/io.github.rsyumi.risunest/native-file-jobs/screenshot-output/11111111-1111-4111-8111-111111111111/archive.zip.part',
                    warningCodes: [],
                }
            }
            if (command === 'native_file_job_screenshot_output_cancel') {
                sourceAvailable = false
                return 'requested'
            }
            if (command === 'native_file_job_screenshot_output_release') {
                sourceAvailable = false
                return undefined
            }
            return undefined
        })
        dependencies.copyToAndroidSaf.mockImplementation(({ signal }) => new Promise<{
            requestId: string
            bytes: number
            warningCodes: string[]
        }>((_resolve, reject) => {
            signal?.addEventListener('abort', () => reject(sourceAvailable
                ? new DOMException('cancelled', 'AbortError')
                : new Error('destination-write-failed')),
            { once: true })
        }))
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)

        const close = writer.close()
        await vi.waitFor(() => expect(dependencies.copyToAndroidSaf).toHaveBeenCalledOnce())
        const abort = writer.abort()

        await expect(close).rejects.toMatchObject({ name: 'AbortError' })
        await expect(abort).rejects.toMatchObject({ name: 'AbortError' })
        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_release')).toHaveLength(1)
        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_cancel')).toHaveLength(0)
    })

    it('preserves a cancelled SAF partial-file warning through the streaming archive', async () => {
        const { dependencies } = androidHarness()
        dependencies.copyToAndroidSaf.mockImplementation(({ signal }) => new Promise((
            _resolve,
            reject,
        ) => {
            signal?.addEventListener('abort', () => reject(Object.assign(
                new DOMException('copy cancelled', 'AbortError'),
                {
                    requestId: 'saf-request-1',
                    warningCodes: ['partial-destination-may-remain'],
                },
            )), { once: true })
        }))
        const controller = new AbortController()
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)
        const archive = createStreamingScreenshotArchive(writer)
        await archive.addPage(1, new Blob(['page'], { type: 'image/png' }))

        const close = archive.close(controller.signal)
        await vi.waitFor(() => expect(dependencies.copyToAndroidSaf).toHaveBeenCalledOnce())
        controller.abort()

        await expect(close).rejects.toMatchObject({
            name: 'AbortError',
            warningCodes: ['partial-destination-may-remain'],
        })
        await expect(archive.close()).rejects.toThrow('aborted')
    })

    it('cancels native validation before the SAF picker is started', async () => {
        const { calls, dependencies } = androidHarness()
        let rejectPublish!: (error: unknown) => void
        dependencies.invoke.mockImplementation((command, args) => {
            calls.push([command, args])
            if (command === 'native_file_job_screenshot_output_start') {
                return Promise.resolve({ jobId: '11111111-1111-4111-8111-111111111111' })
            }
            if (command === 'native_file_job_screenshot_output_publish') {
                return new Promise((_resolve, reject) => {
                    rejectPublish = reject
                })
            }
            if (command === 'native_file_job_screenshot_output_cancel') {
                rejectPublish(new DOMException('cancelled', 'AbortError'))
                return Promise.resolve('requested')
            }
            if (command === 'native_file_job_screenshot_output_release') return Promise.resolve()
            return Promise.resolve()
        })
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)

        const close = writer.close()
        const abort = writer.abort()

        await expect(close).rejects.toMatchObject({ name: 'AbortError' })
        await expect(abort).rejects.toMatchObject({ name: 'AbortError' })
        expect(dependencies.copyToAndroidSaf).not.toHaveBeenCalled()
        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_cancel')).toHaveLength(1)
    })

    it('releases the spool and preserves partial destination warnings after SAF failure', async () => {
        const { calls, dependencies } = androidHarness()
        const failure = Object.assign(new Error('provider stopped'), {
            warningCodes: ['partial-destination-may-remain'],
        })
        dependencies.copyToAndroidSaf.mockRejectedValueOnce(failure)
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)

        await expect(writer.close()).rejects.toBe(failure)

        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_release')).toHaveLength(1)
        expect(failure.warningCodes).toContain('partial-destination-may-remain')
        expect(describeScreenshotPublicationError(
            failure,
            'A partial file may remain at the selected destination.',
        )).toBe(
            'provider stopped A partial file may remain at the selected destination.',
        )
    })

    it('retries a transient spool cleanup failure when archive abort reconciles the error', async () => {
        const { calls, dependencies } = androidHarness()
        dependencies.copyToAndroidSaf.mockRejectedValueOnce(new Error('provider stopped'))
        let releases = 0
        dependencies.invoke.mockImplementation(async (command, args) => {
            calls.push([command, args])
            if (command === 'native_file_job_screenshot_output_start') {
                return { jobId: '11111111-1111-4111-8111-111111111111' }
            }
            if (command === 'native_file_job_screenshot_output_publish') {
                return {
                    bytes: 7,
                    sourcePath: '/data/user/0/io.github.rsyumi.risunest/native-file-jobs/screenshot-output/11111111-1111-4111-8111-111111111111/archive.zip.part',
                    warningCodes: [],
                }
            }
            if (command === 'native_file_job_screenshot_output_release') {
                if (releases++ === 0) throw new Error('bridge unavailable')
                return undefined
            }
            return undefined
        })
        const writer = await createAndroidScreenshotArchiveWriter('chat.zip', dependencies)

        await expect(writer.close()).rejects.toThrow('provider stopped')
        await expect(writer.abort()).rejects.toThrow('provider stopped')

        expect(calls.filter(([command]) =>
            command === 'native_file_job_screenshot_output_release')).toHaveLength(2)
        expect(dependencies.warn).toHaveBeenCalledWith('cleanup-failed')
    })
})

describe('Android screenshot publication recovery', () => {
    const terminal = {
        requestId: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
        exportId: '11111111-1111-4111-8111-111111111111',
        sourceKind: 'screenshot' as const,
        state: 'succeeded' as const,
        bytes: 7,
        warningCodes: ['android-saf-provider-not-atomic'],
    }

    it('releases a recovered screenshot handoff before acknowledging its terminal journal', async () => {
        const order: string[] = []
        const result = await recoverAndroidScreenshotPublication({
            getStatus: () => JSON.stringify(terminal),
            invoke: vi.fn(async () => { order.push('release') }),
            acknowledgeAndroidSafExport: vi.fn(() => {
                order.push('acknowledge')
                return true
            }),
        })

        expect(result).toEqual(terminal)
        expect(order).toEqual(['release', 'acknowledge'])
    })

    it('keeps a recovered terminal journal when exact source cleanup fails', async () => {
        const acknowledge = vi.fn(() => true)
        await expect(recoverAndroidScreenshotPublication({
            getStatus: () => JSON.stringify(terminal),
            invoke: vi.fn(async () => { throw new Error('cleanup failed') }),
            acknowledgeAndroidSafExport: acknowledge,
        })).rejects.toThrow('cleanup failed')

        expect(acknowledge).not.toHaveBeenCalled()
    })

    it('ignores recovered RisuSave terminals', async () => {
        const invoke = vi.fn()
        const result = await recoverAndroidScreenshotPublication({
            getStatus: () => JSON.stringify({ ...terminal, sourceKind: 'risuSave' }),
            invoke,
            acknowledgeAndroidSafExport: vi.fn(),
        })

        expect(result).toBeNull()
        expect(invoke).not.toHaveBeenCalled()
    })

    it('recovers a journal that becomes terminal after bootstrap', async () => {
        let destinationListener!: (event: typeof terminal) => void
        const invoke = vi.fn(async () => undefined)
        const acknowledge = vi.fn(() => true)
        const onTerminal = vi.fn()
        const onError = vi.fn()
        const dispose = listenRecoveredAndroidScreenshotPublications(
            onTerminal,
            onError,
            {
                getStatus: () => null,
                invoke,
                acknowledgeAndroidSafExport: acknowledge,
                listen: (listener) => {
                    destinationListener = listener as (event: typeof terminal) => void
                    return vi.fn()
                },
                isActive: () => false,
            },
        )

        destinationListener(terminal)
        await vi.waitFor(() => expect(onTerminal).toHaveBeenCalledExactlyOnceWith(terminal))

        expect(invoke).toHaveBeenCalledExactlyOnceWith(
            'native_file_job_screenshot_output_release',
            { jobId: terminal.exportId },
        )
        expect(acknowledge).toHaveBeenCalledExactlyOnceWith(terminal.requestId)
        expect(onError).not.toHaveBeenCalled()
        dispose()
    })

    it('does not steal a terminal event from the live screenshot writer', async () => {
        let destinationListener!: (event: typeof terminal) => void
        const invoke = vi.fn()
        const onTerminal = vi.fn()
        const dispose = listenRecoveredAndroidScreenshotPublications(
            onTerminal,
            vi.fn(),
            {
                getStatus: () => JSON.stringify(terminal),
                invoke,
                acknowledgeAndroidSafExport: vi.fn(),
                listen: (listener) => {
                    destinationListener = listener as (event: typeof terminal) => void
                    return vi.fn()
                },
                isActive: (requestId) => requestId === terminal.requestId,
            },
        )

        destinationListener(terminal)
        await Promise.resolve()
        await Promise.resolve()

        expect(invoke).not.toHaveBeenCalled()
        expect(onTerminal).not.toHaveBeenCalled()
        dispose()
    })
})
