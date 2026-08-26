import { describe, expect, it, vi } from 'vitest'

import {
    cancelAndroidSafSource,
    consumeAndroidSpoolBatch,
    copyNativeExportToAndroidSaf,
    getActiveAndroidSafSourceRequestIds,
    listenAndroidSpoolBatches,
    type AndroidSafDestinationEvent,
} from './androidSafBridge'

describe('Android SAF bridge', () => {
    it('subscribes before consuming the replayed ready batch and removes the listener', async () => {
        const listeners = new Set<(event: Event) => void>()
        const batches: unknown[] = []
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
                initialBatch: () => initial,
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
        dispose()
        expect(listeners.size).toBe(0)
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

    it('forwards cancellation to Kotlin and releases its event listener', async () => {
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

        await expect(promise).rejects.toMatchObject({ name: 'AbortError' })
        expect(cancelExport).toHaveBeenCalledExactlyOnceWith('request-3')
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
