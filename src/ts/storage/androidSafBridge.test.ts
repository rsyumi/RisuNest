import { describe, expect, it, vi } from 'vitest'

import {
    consumeAndroidSpoolBatch,
    copyNativeExportToAndroidSaf,
    type AndroidSafDestinationEvent,
} from './androidSafBridge'

describe('Android SAF bridge', () => {
    it('passes ready spool tokens to native jobs without file reads or byte payloads', async () => {
        const restore = vi.fn(async () => undefined)
        const unsupported = vi.fn()

        await consumeAndroidSpoolBatch({
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
            bridge: { copyExport },
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
})
