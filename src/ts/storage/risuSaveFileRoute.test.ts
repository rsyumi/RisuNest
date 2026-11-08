import { androidSafReservationFixture } from '../../../tests/support/androidSafReservation'
import { describe, expect, it, vi } from 'vitest'

import {
    NativeFileJobError,
} from './nativeFileJobs'
import {
    importRisuSaveFromPicker,
    exportRisuSaveFromPicker,
    type RisuSaveFileRouteDependencies,
} from './risuSaveFileRoute'
import * as risuSaveFileRoute from './risuSaveFileRoute'

function dependencies(platform: 'native-desktop' | 'web'): RisuSaveFileRouteDependencies {
    return {
        reserveAndroidDestination: androidSafReservationFixture().reserve,
        platform: () => platform,
        runtime: () => ({
            store: {} as never,
            revision: 4,
            getStorageAuthorityEpoch: () => 1,
            withPausedPersistentWrites: async () => { throw new Error('Unexpected route pause') },
            setActivatedLibraryRecoveryLifecycle: () => { throw new Error('Unexpected upstream lifecycle') },
            beginActivatedLibraryGuard: () => { throw new Error('Unexpected route guard') },
            refreshActivatedLibraryUnderPause: async () => { throw new Error('Unexpected route refresh') },
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: 4,
                mutationGeneration: 1,
            })),
            markCommittedWorkingSetRefreshRequired: vi.fn(),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                revision: 4,
                refreshCommittedWorkingSet: vi.fn(async () => undefined),
                release: vi.fn(),
            })),
            replacePersistentDatabase: vi.fn(async () => undefined),
        }),
        chooseNativeImport: vi.fn(async () => 'C:\\chosen\\source.risudat'),
        chooseNativeExport: vi.fn(async () => 'C:\\chosen\\destination.risudat'),
        chooseWebImport: vi.fn(async () => [{
            name: 'source.risudat',
            size: 3,
            arrayBuffer: async () => Uint8Array.from([1, 2, 3]).buffer,
        }]),
        runNativeImport: vi.fn(async (_runtime, _source, options) => {
            try {
                await options.afterRefresh?.()
            }
            catch (error) {
                const committed = new Error('committed refresh failed') as Error & {
                    name: string
                    committedRevision: number
                    recoveryRequired: boolean
                }
                committed.name = 'NativeFileJobActivationCommittedError'
                committed.committedRevision = 5
                committed.recoveryRequired = true
                throw committed
            }
            return {
                revision: 5,
                sourceBytes: 4096,
                sourceFingerprintKind: 'whole-file-sha256' as const, sourceSha256: 'a'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            }
        }),
        runNativeExport: vi.fn(async () => ({
            revision: 4,
            sourceBytes: 8192,
            sourceFingerprintKind: 'whole-file-sha256' as const, sourceSha256: 'b'.repeat(64),
            exportExclusions: { archivedCharacters: 1, collidingPluginValues: 2 },
            characterCount: 2,
            presetCount: 1,
            warningCodes: [],
        })),
        decodeRisuSave: vi.fn(async () => ({ username: 'Web import', characters: [] })),
        collectWebExport: vi.fn(async () => ({ bytes: Uint8Array.from([4, 5, 6]), exclusions: { archivedCharacters: 1, collidingPluginValues: 2 } })),
        downloadWebExport: vi.fn(async () => undefined),
        withFlushedExport: vi.fn(async () => {
            throw new Error('Unexpected managed Android export')
        }),
        copyAndroidExport: vi.fn(async () => {
            throw new Error('Unexpected Android SAF copy')
        }),
        markAndroidExportReady: vi.fn(() => true),
        acknowledgeAndroidExport: vi.fn(() => true),
        reloadPlugins: vi.fn(async () => undefined),
        reloadPluginsAfterNativeRestore: vi.fn(async () => undefined),
    }
}

describe('RisuSave picker route', () => {
    it('releases an owned import even when source inspection fails before the native job', async () => {
        const deps = dependencies('native-desktop')
        deps.platform = () => 'native-ios'
        deps.cleanupNativeImport = vi.fn(async () => {})
        deps.describeNativeSource = vi.fn(async () => {
            throw new Error('source inspection failed')
        })
        await expect(
            importRisuSaveFromPicker({ onSource: vi.fn() }, deps),
        ).rejects.toThrow('source inspection failed')
        expect(deps.runNativeImport).not.toHaveBeenCalled()
        expect(deps.cleanupNativeImport).toHaveBeenCalledExactlyOnceWith(
            'C:\\chosen\\source.risudat',
        )
    })
    function installAndroidExport(
        deps: RisuSaveFileRouteDependencies,
        input: {
            withFlushedExport: (...args: any[]) => Promise<unknown>
            copyAndroidExport: (...args: any[]) => Promise<unknown>
            markAndroidExportReady?: (requestId: string) => boolean
            acknowledgeAndroidExport?: (requestId: string) => boolean | Promise<boolean>
        },
    ): void {
        deps.platform = () => 'native-android' as never
        Object.assign(deps, {
            markAndroidExportReady: vi.fn(() => true),
            acknowledgeAndroidExport: vi.fn(() => true),
            ...input,
        })
    }

    it('does not report, capture or write RisuSave before Android destination selection completes', async () => {
        const picker = androidSafReservationFixture()
        picker.bridge.reserveExport.mockImplementation(() => {})
        const deps = dependencies('native-desktop')
        deps.platform = () => 'native-android'
        deps.reserveAndroidDestination = picker.reserve
        const failure = new Error('synthetic snapshot failure')
        picker.bridge.getExportStatus.mockImplementation(async () => picker.bridge.reserveExport.mock.calls.length ? JSON.stringify({
            requestId: picker.bridge.reserveExport.mock.calls[0][0], state: 'cancelled',
            warningCodes: ['partial-destination-may-remain'],
        }) : null)
        const chosen = vi.fn()
        deps.withFlushedExport = vi.fn(async () => {
            expect(chosen).toHaveBeenCalledOnce()
            throw failure
        })
        const output = exportRisuSaveFromPicker({ onDestinationChosen: chosen }, deps)
        const rejected = expect(output).rejects.toBe(failure)
        await vi.waitFor(() => expect(picker.bridge.reserveExport).toHaveBeenCalled())
        expect(deps.withFlushedExport).not.toHaveBeenCalled()
        expect(chosen).not.toHaveBeenCalled()
        picker.select(picker.bridge.reserveExport.mock.calls[0][0])
        await rejected
        expect(failure).toMatchObject({warningCodes: ['partial-destination-may-remain']})
        expect(picker.bridge.releaseExport).toHaveBeenCalled()
        expect(deps.copyAndroidExport).not.toHaveBeenCalled()
    })

    it('keeps the managed native file alive through the Android SAF terminal without exposing payload bytes', async () => {
        const deps = dependencies('native-desktop')
        const events: string[] = []
        let releaseTerminal!: () => void
        const terminal = new Promise<void>((resolve) => releaseTerminal = resolve)
        const copyAndroidExport = vi.fn(async (request: Record<string, unknown>) => {
            events.push('saf-copy-start')
            expect(request).toEqual(expect.objectContaining({
                sourcePath: '/app/persistent/exports/risusave-a.risudat',
                suggestedName: expect.stringMatching(/^risunest-\d{4}(-\d{2}){5}\.risudat$/),
                deferAcknowledgement: true,
            }))
            expect(JSON.stringify(request)).not.toContain('Uint8Array')
            await terminal
            events.push('saf-terminal')
            return {
                requestId: 'saf-request-1',
                bytes: 8_192,
                warningCodes: ['android-saf-provider-not-atomic'],
            }
        })
        const withFlushedExport = vi.fn(async (
            _runtime: unknown,
            reason: string,
            callback: (pinned: Record<string, unknown>) => Promise<unknown>,
        ) => {
            events.push(`pin:${reason}`)
            try {
                return await callback({
                    revision: 4,
                    withNativeFile: async (
                        options: unknown,
                        nativeCallback: (file: import('./nativePersistentExport').NativePersistentExportFile) => Promise<unknown>,
                    ) => {
                        events.push(`native-file:${JSON.stringify(options)}`)
                        try {
                            return await nativeCallback({
                                path: '/app/persistent/exports/risusave-a.risudat',
                                bytes: 8_192, excludedArchivedCharacterCount: 1, excludedCollidingPluginValueCount: 2,
                            })
                        }
                        finally {
                            events.push('native-file-cleanup')
                        }
                    },
                })
            }
            finally {
                events.push('lease-release')
            }
        })
        const acknowledgeAndroidExport = vi.fn(() => {
            events.push('saf-acknowledge')
            return true
        })
        const markAndroidExportReady = vi.fn(() => {
            events.push('saf-prerequisites-persisted')
            return true
        })
        installAndroidExport(deps, {
            withFlushedExport,
            copyAndroidExport,
            markAndroidExportReady,
            acknowledgeAndroidExport,
        })

        const pending = exportRisuSaveFromPicker({ omitAccount: true }, deps)
        await vi.waitFor(() => expect(events).toEqual([
            'pin:risu-save-file-export',
            'native-file:{"omitAccount":true}',
            'saf-copy-start',
        ]))
        expect(deps.collectWebExport).not.toHaveBeenCalled()
        expect(deps.downloadWebExport).not.toHaveBeenCalled()

        releaseTerminal()

        await expect(pending).resolves.toEqual({
            mode: 'native',
            bytes: 8_192,
            exclusions: { archivedCharacters: 1, collidingPluginValues: 2 },
            warningCodes: ['android-saf-provider-not-atomic'],
        })
        expect(events).toEqual([
            'pin:risu-save-file-export',
            'native-file:{"omitAccount":true}',
            'saf-copy-start',
            'saf-terminal',
            'native-file-cleanup',
            'lease-release',
            'saf-prerequisites-persisted',
            'saf-acknowledge',
        ])
        expect(markAndroidExportReady).toHaveBeenCalledExactlyOnceWith('saf-request-1')
        expect(acknowledgeAndroidExport).toHaveBeenCalledExactlyOnceWith('saf-request-1')
    })

    // Any post-copy rejection out of the awaited withFlushedExport call (managed source
    // cleanup failure, pinned revision release failure, ...) is indistinguishable at the
    // route boundary and must leave the terminal unacknowledged; the cleanup-vs-release
    // distinction itself is covered by the lease and cleanup implementation tests in
    // nativePersistentExport.test.ts.
    it('leaves the SAF terminal replayable when managed source cleanup fails', async () => {
        const deps = dependencies('native-desktop')
        const markAndroidExportReady = vi.fn(() => true)
        const acknowledgeAndroidExport = vi.fn(() => true)
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => await callback({
                withNativeFile: async (_options: unknown, nativeCallback: Function) => {
                    await nativeCallback({
                        path: '/app/persistent/exports/source.risudat',
                        bytes: 10,
                    })
                    throw new Error('managed source cleanup failed')
                },
            }),
            copyAndroidExport: async () => ({
                requestId: 'saf-request-cleanup',
                bytes: 10,
                warningCodes: [],
            }),
            markAndroidExportReady,
            acknowledgeAndroidExport,
        })

        await expect(exportRisuSaveFromPicker({}, deps)).rejects.toThrow(
            'managed source cleanup failed',
        )
        expect(markAndroidExportReady).not.toHaveBeenCalled()
        expect(acknowledgeAndroidExport).not.toHaveBeenCalled()

        const replayAck = vi.fn(() => true)
        expect(await risuSaveFileRoute.recoverAndroidRisuSavePublication(JSON.stringify({
            requestId: '77777777-7777-4777-8777-777777777777',
            exportId: '88888888-8888-4888-8888-888888888888',
            sourceKind: 'risuSave',
            state: 'succeeded',
            publicationPrerequisitesComplete: false,
            warningCodes: [],
        }), replayAck)).toBeNull()
        expect(replayAck).not.toHaveBeenCalled()
    })

    it('propagates acknowledgement failure while retaining a renderer recovery retry', async () => {
        const deps = dependencies('native-desktop')
        const markAndroidExportReady = vi.fn(() => true)
        const acknowledgeAndroidExport = vi.fn(async () => false)
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => await callback({
                withNativeFile: async (_options: unknown, nativeCallback: Function) =>
                    await nativeCallback({
                        path: '/app/persistent/exports/source.risudat',
                        bytes: 10,
                    }),
            }),
            copyAndroidExport: async () => ({
                requestId: '11111111-1111-4111-8111-111111111111',
                bytes: 10,
                warningCodes: [],
            }),
            markAndroidExportReady,
            acknowledgeAndroidExport,
        })

        await expect(exportRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            code: 'acknowledgement-failed',
            requestId: '11111111-1111-4111-8111-111111111111',
        })
        expect(markAndroidExportReady).toHaveBeenCalledExactlyOnceWith(
            '11111111-1111-4111-8111-111111111111',
        )

        const recover = risuSaveFileRoute.recoverAndroidRisuSavePublication
        const retry = vi.fn(async () => true)
        expect(await recover(JSON.stringify({
            requestId: '11111111-1111-4111-8111-111111111111',
            exportId: '22222222-2222-4222-8222-222222222222',
            sourceKind: 'risuSave',
            state: 'succeeded',
            publicationPrerequisitesComplete: true,
            bytes: 10,
            warningCodes: [],
        }), retry)).toMatchObject({ state: 'succeeded' })
        expect(retry).toHaveBeenCalledExactlyOnceWith(
            '11111111-1111-4111-8111-111111111111',
        )
    })

    it.each(['cancelled', 'failed'])('recovers an unstarted RisuSave reservation after %s renderer loss', async (state) => {
        let saved: unknown = {
            requestId: '11111111-1111-4111-8111-111111111111',
            exportId: '11111111-1111-4111-8111-111111111111',
            sourceKind: 'risuSave', state, copyStarted: false,
            publicationPrerequisitesComplete: true, warningCodes: [],
        }
        const recovered = await risuSaveFileRoute.recoverAndroidRisuSavePublication(JSON.stringify(saved), async (id) => {
            if (id !== '11111111-1111-4111-8111-111111111111') return false
            saved = null
            return true
        })
        expect(recovered).toMatchObject({state, copyStarted: false})
        expect(saved).toBeNull()
    })

    it('consumes a replayable RisuSave terminal when a renderer starts again', async () => {
        const listenRecovered = (risuSaveFileRoute as unknown as {
            listenRecoveredAndroidRisuSavePublications(
                onTerminal: (terminal: unknown) => void,
                onError: (error: unknown) => void,
                dependencies: {
                    getStatus(): string | null
                    acknowledge(requestId: string): boolean
                    listen(listener: (event: unknown) => void): () => void
                    isActive(requestId: string): boolean
                },
            ): () => void
        }).listenRecoveredAndroidRisuSavePublications
        expect(listenRecovered).toBeTypeOf('function')
        const terminal = {
            requestId: '55555555-5555-4555-8555-555555555555',
            exportId: '66666666-6666-4666-8666-666666666666',
            sourceKind: 'risuSave',
            state: 'failed',
            code: 'destination-interrupted',
            copyStarted: true,
            publicationPrerequisitesComplete: true,
            warningCodes: ['partial-destination-may-remain'],
        }
        const acknowledge = vi.fn((_requestId: string) => true)
        const onTerminal = vi.fn()
        const onError = vi.fn()
        const disposeListener = vi.fn()
        let replay!: (event: unknown) => void

        const dispose = listenRecovered(onTerminal, onError, {
            getStatus: () => JSON.stringify(terminal),
            acknowledge,
            listen: vi.fn(listener => { replay = listener; return disposeListener }),
            isActive: () => false,
        })

        await vi.waitFor(() => expect(onTerminal).toHaveBeenCalledExactlyOnceWith(terminal))
        replay(terminal)
        replay(terminal)
        const nextTerminal = {...terminal, requestId: '77777777-7777-4777-8777-777777777777'}
        replay(nextTerminal)
        await vi.waitFor(() => expect(onTerminal).toHaveBeenLastCalledWith(nextTerminal))
        expect(onTerminal.mock.calls.filter(([value]) => value.requestId === terminal.requestId)).toHaveLength(1)
        expect(acknowledge.mock.calls.filter(([id]) => id === terminal.requestId)).toHaveLength(1)
        expect(onError).not.toHaveBeenCalled()
        dispose()
        expect(disposeListener).toHaveBeenCalledOnce()
    })

    it('fails closed across WebView recreation until durable prerequisite proof exists', async () => {
        const terminal = {
            requestId: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb',
            exportId: 'cccccccc-cccc-4ccc-8ccc-cccccccccccc',
            sourceKind: 'risuSave' as const,
            state: 'succeeded' as const,
            publicationPrerequisitesComplete: false,
            warningCodes: [],
        }
        const acknowledge = vi.fn(() => true)
        const onTerminal = vi.fn()
        const dependencies = {
            getStatus: () => JSON.stringify(terminal),
            acknowledge,
            listen: vi.fn(() => vi.fn()),
            isActive: () => false,
        }

        const disposeBeforeProof = risuSaveFileRoute.listenRecoveredAndroidRisuSavePublications(
            onTerminal,
            vi.fn(),
            dependencies,
        )
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(acknowledge).not.toHaveBeenCalled()
        expect(onTerminal).not.toHaveBeenCalled()
        disposeBeforeProof()

        terminal.publicationPrerequisitesComplete = true
        const disposeAfterProof = risuSaveFileRoute.listenRecoveredAndroidRisuSavePublications(
            onTerminal,
            vi.fn(),
            dependencies,
        )
        await vi.waitFor(() => expect(onTerminal).toHaveBeenCalledOnce())
        expect(acknowledge).toHaveBeenCalledExactlyOnceWith(terminal.requestId)
        disposeAfterProof()
    })

    it('does not acknowledge when durable prerequisite proof cannot be persisted', async () => {
        const deps = dependencies('native-desktop')
        const acknowledgeAndroidExport = vi.fn(() => true)
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => await callback({
                withNativeFile: async (_options: unknown, nativeCallback: Function) =>
                    await nativeCallback({
                        path: '/app/persistent/exports/source.risudat',
                        bytes: 10,
                    }),
            }),
            copyAndroidExport: async () => ({
                requestId: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd',
                bytes: 10,
                warningCodes: [],
            }),
            markAndroidExportReady: () => false,
            acknowledgeAndroidExport,
        })

        await expect(exportRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            code: 'prerequisite-proof-failed',
            requestId: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd',
        })
        expect(acknowledgeAndroidExport).not.toHaveBeenCalled()
    })

    it('fails closed when Android SAF reports a different byte count', async () => {
        const deps = dependencies('native-desktop')
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => await callback({
                withNativeFile: async (_options: unknown, nativeCallback: Function) =>
                    await nativeCallback({ path: '/app/persistent/exports/source.risudat', bytes: 10, excludedArchivedCharacterCount: 1, excludedCollidingPluginValueCount: 2 }),
            }),
            copyAndroidExport: async () => ({
                requestId: 'saf-request-2',
                bytes: 9,
                warningCodes: ['android-saf-provider-not-atomic'],
            }),
        })

        await expect(exportRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            code: 'byte-count-mismatch',
            warningCodes: [
                'android-saf-provider-not-atomic',
                'partial-destination-may-remain',
            ],
        })
        expect(deps.collectWebExport).not.toHaveBeenCalled()
    })

    it('reserves the mandatory partial-destination warning within the 16-code bound', async () => {
        const deps = dependencies('native-desktop')
        const optionalWarnings = Array.from({ length: 16 }, (_, index) => `optional-${index}`)
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => await callback({
                withNativeFile: async (_options: unknown, nativeCallback: Function) =>
                    await nativeCallback({
                        path: '/app/persistent/exports/source.risudat',
                        bytes: 10,
                    }),
            }),
            copyAndroidExport: async () => ({
                requestId: '44444444-4444-4444-8444-444444444444',
                bytes: 9,
                warningCodes: optionalWarnings,
            }),
        })

        await expect(exportRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            warningCodes: [...optionalWarnings.slice(0, 15), 'partial-destination-may-remain'],
        })
    })

    it('deduplicates Android SAF warning codes', async () => {
        const deps = dependencies('native-desktop')
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => await callback({
                withNativeFile: async (_options: unknown, nativeCallback: Function) =>
                    await nativeCallback({ path: '/app/persistent/exports/source.risudat', bytes: 10, excludedArchivedCharacterCount: 1, excludedCollidingPluginValueCount: 2 }),
            }),
            copyAndroidExport: async () => ({
                requestId: 'saf-request-3',
                bytes: 10,
                warningCodes: [
                    'android-saf-provider-not-atomic',
                    'android-saf-provider-not-atomic',
                    'partial-destination-may-remain',
                ],
            }),
        })

        await expect(exportRisuSaveFromPicker({}, deps)).resolves.toEqual({
            mode: 'native',
            bytes: 10,
            exclusions: { archivedCharacters: 1, collidingPluginValues: 2 },
            warningCodes: [
                'android-saf-provider-not-atomic',
                'partial-destination-may-remain',
            ],
        })
    })

    it('releases the managed file only after a cancelled SAF terminal preserves its partial warning', async () => {
        const deps = dependencies('native-desktop')
        const events: string[] = []
        const cancellation = Object.assign(
            new DOMException('copy cancelled', 'AbortError'),
            {
                requestId: '33333333-3333-4333-8333-333333333333',
                warningCodes: [
                    ...Array.from({ length: 16 }, (_, index) => `optional-${index}`),
                    'partial-destination-may-remain',
                ],
            },
        )
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => {
                try {
                    return await callback({
                        withNativeFile: async (_options: unknown, nativeCallback: Function) => {
                            try {
                                return await nativeCallback({
                                    path: '/app/persistent/exports/source.risudat',
                                    bytes: 10,
                                })
                            }
                            finally {
                                events.push('native-file-cleanup')
                            }
                        },
                    })
                }
                finally {
                    events.push('lease-release')
                }
            },
            copyAndroidExport: async () => {
                events.push('saf-cancelled-terminal')
                throw cancellation
            },
            markAndroidExportReady: (requestId) => {
                events.push(`saf-prerequisites-persisted:${requestId}`)
                return true
            },
            acknowledgeAndroidExport: (requestId) => {
                events.push(`saf-acknowledge:${requestId}`)
                return true
            },
        })

        await expect(exportRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            name: 'AbortError',
            warningCodes: [
                ...Array.from({ length: 15 }, (_, index) => `optional-${index}`),
                'partial-destination-may-remain',
            ],
        })
        expect(events).toEqual([
            'saf-cancelled-terminal',
            'native-file-cleanup',
            'lease-release',
            'saf-prerequisites-persisted:33333333-3333-4333-8333-333333333333',
            'saf-acknowledge:33333333-3333-4333-8333-333333333333',
        ])
    })

    it('alerts the mandatory partial warning for an AbortError exactly once', () => {
        const alertPartialDestinationWarning = (risuSaveFileRoute as unknown as {
            alertPartialDestinationWarning(
                error: unknown,
                warning: string,
                alert: (message: string) => void,
            ): boolean
        }).alertPartialDestinationWarning
        expect(alertPartialDestinationWarning).toBeTypeOf('function')
        const alert = vi.fn()
        const error = Object.assign(new DOMException('cancelled', 'AbortError'), {
            warningCodes: [
                ...Array.from({ length: 16 }, (_, index) => `optional-${index}`),
                'partial-destination-may-remain',
            ],
        })

        expect(alertPartialDestinationWarning(error, 'A partial file may remain.', alert))
            .toBe(true)
        expect(alert).toHaveBeenCalledExactlyOnceWith('A partial file may remain.')
    })

    it('fails closed when the pinned Android revision has no managed native file', async () => {
        const deps = dependencies('native-desktop')
        const copyAndroidExport = vi.fn()
        installAndroidExport(deps, {
            withFlushedExport: async (_runtime, _reason, callback) => await callback({
                revision: 4,
                collectBytes: vi.fn(async () => Uint8Array.from([1, 2, 3])),
            }),
            copyAndroidExport,
        })

        await expect(exportRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            code: 'capability-unavailable',
        })
        expect(copyAndroidExport).not.toHaveBeenCalled()
        expect(deps.collectWebExport).not.toHaveBeenCalled()
        expect(deps.downloadWebExport).not.toHaveBeenCalled()
    })

    it('passes only the selected desktop path to native import and refreshes through the job facade', async () => {
        const deps = dependencies('native-desktop')

        const result = await importRisuSaveFromPicker({}, deps)

        expect(result?.mode).toBe('native')
        expect(deps.runNativeImport).toHaveBeenCalledWith(
            expect.objectContaining({ revision: 4 }),
            { type: 'desktopPath', path: 'C:\\chosen\\source.risudat' },
            expect.objectContaining({
                afterRefresh: deps.reloadPluginsAfterNativeRestore,
                onStatus: undefined,
                signal: undefined,
            }),
        )
        expect(deps.decodeRisuSave).not.toHaveBeenCalled()
        expect(deps.chooseWebImport).not.toHaveBeenCalled()
        expect(deps.reloadPluginsAfterNativeRestore).toHaveBeenCalledOnce()
        expect(deps.reloadPlugins).not.toHaveBeenCalled()
    })

    it('publishes desktop export through the native job with omit-account unchanged', async () => {
        const deps = dependencies('native-desktop')
        const chosen = vi.fn()

        const result = await exportRisuSaveFromPicker({ omitAccount: true, onDestinationChosen: chosen }, deps)

        expect(chosen).toHaveBeenCalledOnce()
        expect(chosen.mock.invocationCallOrder[0]).toBeGreaterThan(vi.mocked(deps.chooseNativeExport).mock.invocationCallOrder[0])
        expect(chosen.mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(deps.runNativeExport).mock.invocationCallOrder[0])
        expect(result?.mode).toBe('native')
        expect(deps.runNativeExport).toHaveBeenCalledWith(
            expect.objectContaining({ revision: 4 }),
            'C:\\chosen\\destination.risudat',
            expect.objectContaining({ omitAccount: true }),
        )
        expect(deps.collectWebExport).not.toHaveBeenCalled()
        expect(deps.downloadWebExport).not.toHaveBeenCalled()
    })

    it('keeps the JavaScript codec only as the browser Web compatibility fallback', async () => {
        const deps = dependencies('web')
        const runtime = deps.runtime()
        deps.runtime = () => runtime

        const imported = await importRisuSaveFromPicker({}, deps)
        const exported = await exportRisuSaveFromPicker({ omitAccount: false }, deps)

        expect(imported?.mode).toBe('web')
        expect(exported?.mode).toBe('web')
        expect(deps.decodeRisuSave).toHaveBeenCalledWith(Uint8Array.from([1, 2, 3]))
        expect(runtime.replacePersistentDatabase).toHaveBeenCalledWith(
            { username: 'Web import', characters: [], explicitGlobalChatVariables: {} },
            'risu-save-file-import',
            { authoritative: true, upstreamImport: true },
        )
        expect(deps.collectWebExport).toHaveBeenCalledWith(false)
        expect(deps.downloadWebExport).toHaveBeenCalledWith(
            expect.stringMatching(/^risunest-.*\.risudat$/),
            Uint8Array.from([4, 5, 6]),
        )
        expect(deps.runNativeImport).not.toHaveBeenCalled()
        expect(deps.runNativeExport).not.toHaveBeenCalled()
    })

    it('reports upstream cold plugin storage in a browser save as left out instead of storing it', async () => {
        const deps = dependencies('web')
        const runtime = deps.runtime()
        deps.runtime = () => runtime
        vi.mocked(deps.decodeRisuSave).mockResolvedValueOnce({
            username: 'Web import',
            characters: [],
            pluginCustomStorage: { kept: 0, _coldpluginIndex: 'index-id', _coldplugin: { legacy: 'legacy-id' } },
        })

        const imported = await importRisuSaveFromPicker({}, deps)

        expect(imported?.warningCodes).toEqual(['upstream-restore-losses'])
        expect(vi.mocked(runtime.replacePersistentDatabase).mock.calls[0][0].pluginCustomStorage).toEqual({ kept: 0 })
    })

    it.each(['native-desktop', 'native-ios'] as const)('retains native failure and source cleanup on %s without a browser read', async platform => {
        for (const code of ['capability-unavailable', 'unsupported-format']) {
            const deps = dependencies('native-desktop')
            deps.platform = () => platform
            deps.cleanupNativeImport = vi.fn(async () => {})
            const failure = new NativeFileJobError(code, 'Synthetic native failure')
            vi.mocked(deps.runNativeImport).mockRejectedValueOnce(failure)
            await expect(importRisuSaveFromPicker({}, deps)).rejects.toBe(failure)
            expect(deps.chooseWebImport).not.toHaveBeenCalled()
            expect(deps.decodeRisuSave).not.toHaveBeenCalled()
            expect(deps.cleanupNativeImport).toHaveBeenCalledOnce()
        }
    })

    it('reports the picked desktop file through the source describer when one is configured', async () => {
        const deps = dependencies('native-desktop')
        deps.describeNativeSource = vi.fn(async (path) => ({ name: `described:${path}`, bytes: 42 }))
        const onSource = vi.fn()

        await importRisuSaveFromPicker({ onSource }, deps)

        expect(onSource).toHaveBeenCalledExactlyOnceWith({ name: 'described:C:\\chosen\\source.risudat', bytes: 42 })
    })

    it('walks the web import through reading, decoding, activation, and plugin reload stages', async () => {
        const deps = dependencies('web')
        const statuses: Array<{ stage?: string; completed: number; total?: number }> = []
        const onSource = vi.fn()

        const result = await importRisuSaveFromPicker({
            onStatus: (status) => statuses.push({
                stage: status.detail?.stage,
                completed: status.progress.completedBytes,
                total: status.progress.totalBytes,
            }),
            onSource,
        }, deps)

        expect(result).toEqual({ mode: 'web', warningCodes: [], bytes: 3 })
        expect(onSource).toHaveBeenCalledExactlyOnceWith({ name: 'source.risudat', bytes: 3 })
        expect(statuses).toEqual([
            { stage: 'reading-database', completed: 0, total: 3 },
            { stage: 'decoding-database', completed: 3, total: 3 },
            { stage: 'activating', completed: 3, total: 3 },
            { stage: 'reloading-plugins', completed: 3, total: 3 },
        ])
    })

    it('does not fall back for invalid block input', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.runNativeImport).mockRejectedValueOnce(
            new NativeFileJobError('corrupt-input', 'invalid gzip stream'),
        )

        await expect(importRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            code: 'corrupt-input',
        })
        expect(deps.chooseWebImport).not.toHaveBeenCalled()
        expect(deps.decodeRisuSave).not.toHaveBeenCalled()
    })

    it('reports plugin refresh failure as post-commit recovery instead of restore failure', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.reloadPluginsAfterNativeRestore).mockRejectedValueOnce(
            new Error('plugin refresh failed'),
        )

        await expect(importRisuSaveFromPicker({}, deps)).rejects.toEqual(
            expect.objectContaining({
                name: 'NativeFileJobActivationCommittedError',
                committedRevision: 5,
                recoveryRequired: true,
            }),
        )
    })

    it('falls back to the JavaScript exporter when the desktop native capability is unavailable', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.runNativeExport).mockRejectedValueOnce(
            new NativeFileJobError('capability-unavailable', 'native jobs unavailable'),
        )

        const result = await exportRisuSaveFromPicker({ omitAccount: true }, deps)

        expect(result?.mode).toBe('web')
        expect(deps.collectWebExport).toHaveBeenCalledWith(true)
        expect(deps.downloadWebExport).toHaveBeenCalledOnce()
    })

    it('does nothing when either picker is cancelled', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.chooseNativeImport).mockResolvedValueOnce(null)
        vi.mocked(deps.chooseNativeExport).mockResolvedValueOnce(null)
        const chosen = vi.fn()

        await expect(importRisuSaveFromPicker({}, deps)).resolves.toBeNull()
        await expect(exportRisuSaveFromPicker({ onDestinationChosen: chosen }, deps)).resolves.toBeNull()
        expect(deps.runNativeImport).not.toHaveBeenCalled()
        expect(deps.runNativeExport).not.toHaveBeenCalled()
        expect(chosen).not.toHaveBeenCalled()

        // Backing out of the Android save picker ends the same way, while a cancelled export still rejects.
        const picker = androidSafReservationFixture()
        picker.bridge.reserveExport.mockImplementation((requestId: string) => { picker.backOut(requestId) })
        deps.platform = () => 'native-android'
        deps.reserveAndroidDestination = picker.reserve
        deps.withFlushedExport = vi.fn()
        await expect(exportRisuSaveFromPicker({ onDestinationChosen: chosen }, deps)).resolves.toBeNull()
        expect(picker.bridge.reserveExport).toHaveBeenCalledOnce()
        expect(deps.withFlushedExport).not.toHaveBeenCalled()
        expect(chosen).not.toHaveBeenCalled()
        await expect(exportRisuSaveFromPicker({ signal: AbortSignal.abort() }, deps))
            .rejects.toMatchObject({ name: 'AbortError' })
    })
})
