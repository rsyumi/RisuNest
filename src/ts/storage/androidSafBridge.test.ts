import { describe, expect, it, vi } from 'vitest'

import {
    cancelAndroidSafSource,
    consumeAndroidSpoolBatch,
    copyNativeExportToAndroidSaf,
    discardAndroidSafSource,
    getActiveAndroidSafSourceRequestIds,
    isAndroidSafFileJobsEnabled,
    listenAndroidSpoolBatches,
    type AndroidSafDestinationEvent,
} from './androidSafBridge'

describe('Android SAF bridge', () => {
    it('reports SAF file jobs enabled only when the native bridge is installed', () => {
        expect(isAndroidSafFileJobsEnabled(undefined)).toBe(false)
        expect(isAndroidSafFileJobsEnabled({})).toBe(true)
    })

    it('subscribes before consuming the replayed ready batch and removes the listener', async () => {
        const listeners = new Set<(event: Event) => void>()
        const batches: unknown[] = []
        let pendingClears = 0
        const initial = {
            requestId: 'initial-request',
            ready: [{
                token: '11111111-1111-4111-8111-111111111111',
                displayName: 'initial.risudat',
                bytes: 10,
                totalBytes: 10,
            }],
            failures: [],
        }

        const dispose = listenAndroidSpoolBatches(
            (batch) => batches.push(batch),
            {
                takePendingBatch: () => initial,
                clearPendingBatch: () => pendingClears += 1,
                addEventListener: (_name, listener) => listeners.add(listener),
                removeEventListener: (_name, listener) => listeners.delete(listener),
            },
        )
        const eventBatch = {
            requestId: 'event-request',
            ready: [],
            failures: [{ displayName: 'broken.risudat', code: 'source-read-failed' }],
        }
        for (const listener of listeners) {
            listener(new CustomEvent('risu-android-spool-ready', { detail: eventBatch }))
        }
        await Promise.resolve()

        expect(batches).toEqual([eventBatch, initial])
        expect(pendingClears).toBe(1)
        dispose()
        expect(listeners.size).toBe(0)
    })

    it('discards a ready source through the token-only native bridge', () => {
        const discardSource = vi.fn(() => true)

        expect(discardAndroidSafSource(
            '11111111-1111-4111-8111-111111111111',
            { copyExport: vi.fn(), discardSource },
        )).toBe(true)
        expect(discardSource).toHaveBeenCalledWith(
            '11111111-1111-4111-8111-111111111111',
        )
    })

    it('passes ready spool tokens to native jobs without file reads or byte payloads', async () => {
        const restore = vi.fn(async () => undefined)
        const unsupported = vi.fn()

        await consumeAndroidSpoolBatch({
            requestId: 'source-request-1',
            ready: [
                {
                    token: '11111111-1111-4111-8111-111111111111',
                    displayName: 'database.risudat',
                    bytes: 10_000,
                    totalBytes: 10_000,
                },
                {
                    token: '22222222-2222-4222-8222-222222222222',
                    displayName: 'card.charx',
                    bytes: 2_000,
                },
            ],
            failures: [],
        }, { restore, unsupported })

        expect(restore).toHaveBeenCalledExactlyOnceWith({
            source: {
                type: 'androidSpool',
                token: '11111111-1111-4111-8111-111111111111',
            },
            displayName: 'database.risudat',
        })
        expect(unsupported).toHaveBeenCalledExactlyOnceWith({
            token: '22222222-2222-4222-8222-222222222222',
            displayName: 'card.charx',
            bytes: 2_000,
        })
    })

    it('asks the native picker to publish an owned export while keeping bytes outside TypeScript', async () => {
        const listeners = new Set<(event: Event) => void>()
        const acknowledgeExport = vi.fn(() => true)
        const copyExport = vi.fn((requestId: string) => {
            queueMicrotask(() => {
                const detail: AndroidSafDestinationEvent = {
                    requestId,
                    state: 'succeeded',
                    bytes: 4_294_967_296,
                    warningCodes: ['android-saf-provider-not-atomic'],
                }
                for (const listener of listeners) {
                    listener(new CustomEvent('risu-android-saf-destination', { detail }))
                }
            })
        })

        const result = await copyNativeExportToAndroidSaf({
            sourcePath: '/data/user/0/co.aiclient.risu/files/persistent/exports/risusave-a.risudat',
            suggestedName: 'backup.risudat',
        }, {
            createRequestId: () => 'request-1',
            bridge: { copyExport, acknowledgeExport },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })

        expect(copyExport).toHaveBeenCalledExactlyOnceWith(
            'request-1',
            '/data/user/0/co.aiclient.risu/files/persistent/exports/risusave-a.risudat',
            'backup.risudat',
        )
        expect(result).toEqual({
            requestId: 'request-1',
            bytes: 4_294_967_296,
            warningCodes: ['android-saf-provider-not-atomic'],
        })
        expect(acknowledgeExport).toHaveBeenCalledExactlyOnceWith('request-1')
        expect(listeners.size).toBe(0)
    })

    it('preserves provider-limited partial destination warnings on failure', async () => {
        const listeners = new Set<(event: Event) => void>()
        const copyExport = vi.fn((requestId: string) => {
            queueMicrotask(() => {
                const detail: AndroidSafDestinationEvent = {
                    requestId,
                    state: 'failed',
                    code: 'destination-write-failed',
                    message: 'provider stopped',
                    warningCodes: [
                        'android-saf-provider-not-atomic',
                        'partial-destination-may-remain',
                    ],
                }
                for (const listener of listeners) {
                    listener(new CustomEvent('risu-android-saf-destination', { detail }))
                }
            })
        })

        await expect(copyNativeExportToAndroidSaf({
            sourcePath: '/data/user/0/co.aiclient.risu/files/persistent/exports/risusave-a.risudat',
            suggestedName: 'backup.risudat',
        }, {
            createRequestId: () => 'request-2',
            bridge: { copyExport },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })).rejects.toMatchObject({
            name: 'AndroidSafDestinationError',
            code: 'destination-write-failed',
            warningCodes: [
                'android-saf-provider-not-atomic',
                'partial-destination-may-remain',
            ],
        })
        expect(listeners.size).toBe(0)
    })

    it('waits for the native cancelled terminal after forwarding cancellation', async () => {
        const listeners = new Set<(event: Event) => void>()
        const controller = new AbortController()
        const cancelExport = vi.fn()
        const promise = copyNativeExportToAndroidSaf({
            sourcePath: '/data/user/0/co.aiclient.risu/persistent/exports/risusave-a.risudat',
            suggestedName: 'backup.risudat',
            signal: controller.signal,
        }, {
            createRequestId: () => 'request-3',
            bridge: { copyExport: vi.fn(), cancelExport },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })

        controller.abort()
        expect(listeners.size).toBe(1)
        for (const listener of listeners) {
            listener(new CustomEvent('risu-android-saf-destination', { detail: {
                requestId: 'request-3',
                exportId: '11111111-1111-4111-8111-111111111111',
                sourceKind: 'risuSave',
                state: 'cancelled',
                code: 'cancelled',
                warningCodes: [],
            } satisfies AndroidSafDestinationEvent }))
        }

        await expect(promise).rejects.toMatchObject({ name: 'AbortError' })
        expect(cancelExport).toHaveBeenCalledExactlyOnceWith('request-3')
        expect(listeners.size).toBe(0)
    })

    it('can defer terminal acknowledgement until an owned screenshot source is released', async () => {
        const listeners = new Set<(event: Event) => void>()
        const acknowledgeExport = vi.fn(() => true)
        const copyExport = vi.fn((requestId: string) => queueMicrotask(() => {
            for (const listener of listeners) {
                listener(new CustomEvent('risu-android-saf-destination', { detail: {
                    requestId,
                    exportId: '11111111-1111-4111-8111-111111111111',
                    sourceKind: 'screenshot',
                    state: 'succeeded',
                    bytes: 3,
                    warningCodes: [],
                } satisfies AndroidSafDestinationEvent }))
            }
        }))

        const result = await copyNativeExportToAndroidSaf({
            sourcePath: '/data/user/0/co.aiclient.risu/native-file-jobs/screenshot-output/11111111-1111-4111-8111-111111111111/archive.zip.part',
            suggestedName: 'chat.zip',
            deferAcknowledgement: true,
        }, {
            createRequestId: () => 'request-screenshot',
            bridge: { copyExport, acknowledgeExport },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })

        expect(result.requestId).toBe('request-screenshot')
        expect(acknowledgeExport).not.toHaveBeenCalled()
    })

    it('accepts a completed native publication when cancellation arrives too late', async () => {
        const listeners = new Set<(event: Event) => void>()
        const controller = new AbortController()
        const promise = copyNativeExportToAndroidSaf({
            sourcePath: '/data/user/0/co.aiclient.risu/persistent/exports/risusave-a.risudat',
            suggestedName: 'backup.risudat',
            signal: controller.signal,
        }, {
            createRequestId: () => 'request-4',
            bridge: { copyExport: vi.fn(), cancelExport: vi.fn(() => false) },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })

        controller.abort()
        for (const listener of listeners) {
            listener(new CustomEvent('risu-android-saf-destination', { detail: {
                requestId: 'request-4',
                exportId: '11111111-1111-4111-8111-111111111111',
                sourceKind: 'risuSave',
                state: 'succeeded',
                bytes: 12,
                warningCodes: ['android-saf-provider-not-atomic'],
            } satisfies AndroidSafDestinationEvent }))
        }

        await expect(promise).resolves.toMatchObject({ bytes: 12, requestId: 'request-4' })
        expect(listeners.size).toBe(0)
    })

    it('preserves a partial destination warning reported after cancellation', async () => {
        const listeners = new Set<(event: Event) => void>()
        const controller = new AbortController()
        const promise = copyNativeExportToAndroidSaf({
            sourcePath: '/data/user/0/co.aiclient.risu/persistent/exports/risusave-a.risudat',
            suggestedName: 'backup.risudat',
            signal: controller.signal,
        }, {
            createRequestId: () => 'request-5',
            bridge: { copyExport: vi.fn(), cancelExport: vi.fn(() => true) },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })

        controller.abort()
        for (const listener of listeners) {
            listener(new CustomEvent('risu-android-saf-destination', { detail: {
                requestId: 'request-5',
                exportId: '11111111-1111-4111-8111-111111111111',
                sourceKind: 'risuSave',
                state: 'cancelled',
                code: 'cancelled',
                message: 'copy cancelled',
                warningCodes: ['partial-destination-may-remain'],
            } satisfies AndroidSafDestinationEvent }))
        }

        await expect(promise).rejects.toMatchObject({
            name: 'AbortError',
            requestId: 'request-5',
            warningCodes: ['partial-destination-may-remain'],
        })
        expect(listeners.size).toBe(0)
    })

    it('forwards a source spool cancellation request without exposing source bytes', () => {
        const cancelSource = vi.fn()

        cancelAndroidSafSource('source-request-1', { copyExport: vi.fn(), cancelSource })

        expect(cancelSource).toHaveBeenCalledExactlyOnceWith('source-request-1')
    })

    it('can discover and cancel a source request before its first progress event', () => {
        const requestId = '11111111-1111-4111-8111-111111111111'
        const cancelSource = vi.fn()
        const bridge = {
            copyExport: vi.fn(),
            cancelSource,
            getActiveSourceRequestIds: () => JSON.stringify([requestId]),
        }

        const active = getActiveAndroidSafSourceRequestIds(bridge)
        cancelAndroidSafSource(active[0], bridge)

        expect(active).toEqual([requestId])
        expect(cancelSource).toHaveBeenCalledExactlyOnceWith(requestId)
    })

    it('reports matching destination progress and ignores other requests', async () => {
        const listeners = new Map<string, Set<(event: Event) => void>>()
        const onProgress = vi.fn()
        const dispatch = (name: string, detail: unknown) => {
            for (const listener of listeners.get(name) ?? []) {
                listener(new CustomEvent(name, { detail }))
            }
        }
        const copyExport = vi.fn((requestId: string) => {
            queueMicrotask(() => {
                dispatch('risu-android-saf-progress', {
                    requestId: 'another-request',
                    operation: 'destination-copy',
                    copiedBytes: 1,
                    totalBytes: 10,
                    token: null,
                })
                dispatch('risu-android-saf-progress', {
                    requestId,
                    operation: 'destination-copy',
                    copiedBytes: 4,
                    totalBytes: 10,
                    token: null,
                })
                dispatch('risu-android-saf-destination', {
                    requestId,
                    state: 'succeeded',
                    bytes: 10,
                    warningCodes: ['android-saf-provider-not-atomic'],
                })
            })
        })

        await copyNativeExportToAndroidSaf({
            sourcePath: '/data/user/0/co.aiclient.risu/files/persistent/exports/source.risudat',
            suggestedName: 'backup.risudat',
            onProgress,
        }, {
            createRequestId: () => 'destination-request-1',
            bridge: { copyExport },
            addEventListener: (name, listener) => {
                const registered = listeners.get(name) ?? new Set()
                registered.add(listener)
                listeners.set(name, registered)
            },
            removeEventListener: (name, listener) => listeners.get(name)?.delete(listener),
        })

        expect(onProgress).toHaveBeenCalledExactlyOnceWith({
            requestId: 'destination-request-1',
            operation: 'destination-copy',
            copiedBytes: 4,
            totalBytes: 10,
            token: null,
        })
        expect([...listeners.values()].every((registered) => registered.size === 0)).toBe(true)
    })
})
