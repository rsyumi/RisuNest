import { describe, expect, it, vi } from 'vitest'
import { isLibraryFileOperationReserved } from './libraryFileOperation'

import {
    consumeAndroidSpoolBatch,
    copyNativeExportToAndroidSaf,
    discardAndroidSafSource,
    getAndroidSafExportSourceId,
    isAndroidSafFileJobsEnabled,
    listenAndroidSpoolBatches,
    materializeAndroidBackupSource,
    pickAndroidLegacyBackupSource,
    pickAndroidContentSources,
    pickAndroidBackupSource,
    pickAndroidPortableBackupSource,
    type AndroidSafDestinationEvent,
} from './androidSafBridge'

describe('Android SAF bridge', () => {
    it('cancels native materialization and discards a copy completed during cancellation', async () => {
        const controller = new AbortController()
        const onProgress = vi.fn()
        let complete!: (value: string) => void
        const nativeCopy = new Promise<string>(resolve => { complete = resolve })
        const released: string[] = []
        const discarded: string[] = []
        const result = materializeAndroidBackupSource('selected-source', { signal: controller.signal, onProgress }, {
            copyExport() {},
            materializeBackupSource(_token, report) {
                report?.({ copiedBytes: 1024, totalBytes: 2048 })
                return nativeCopy
            },
            discardPortableSource(token) {
                released.push(token)
                complete(JSON.stringify({ ready: [{ token: 'completed-spool' }], failures: [] }))
                return true
            },
            discardSource(token) { discarded.push(token); return true },
        })
        const rejected = expect(result).rejects.toMatchObject({ name: 'AbortError' })
        expect(onProgress).toHaveBeenCalledWith({ requestId: 'selected-source', token: 'selected-source', operation: 'source-copy', copiedBytes: 1024, totalBytes: 2048 })
        controller.abort()
        await rejected
        expect(released).toEqual(['selected-source'])
        expect(discarded).toEqual(['completed-spool'])
    })

    it('does not begin a copy after cancellation or retain its abort listener after success', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        const bridge = {
            copyExport() {},
            materializeBackupSource() {
                commands.push('copy')
                return JSON.stringify({ ready: [{ token: 'copied-source' }], failures: [] })
            },
            discardPortableSource() { commands.push('cancel'); return true },
        }
        await expect(materializeAndroidBackupSource('source', { signal: controller.signal }, bridge))
            .resolves.toEqual({ type: 'androidSpool', token: 'copied-source' })
        controller.abort()
        await expect(materializeAndroidBackupSource('source', { signal: controller.signal }, bridge))
            .rejects.toMatchObject({ name: 'AbortError' })
        expect(commands).toEqual(['copy'])
    })

    // iosFiles.test.ts expects the same reasons for the same native codes.
    it.each([
        ['source-changed', 'source-reselect-required'],
        ['cancelled', 'source-reselect-required'],
        ['spool-create-failed', 'spool-write-failed'],
        ['source-busy', 'source-busy'],
        ['source-read-failed', 'source-read-failed'],
    ])('reports a native %s copy failure with the %s dialog reason', async (nativeCode, code) => {
        await expect(materializeAndroidBackupSource('source', {}, {
            copyExport() {},
            materializeBackupSource: () => JSON.stringify({ ready: [], failures: [{ displayName: 'backup', code: nativeCode }] }),
        })).rejects.toMatchObject({ code, message: expect.stringContaining(nativeCode) })
    })

    it('keeps the source picker reserved until its correlated native result', async () => {
        const listeners = new Map<string, (event: Event) => void>()
        const pending = pickAndroidContentSources({ destination: 'module' }, {
            createRequestId: () => 'selection',
            bridge: { copyExport: vi.fn(), pickContentSource: vi.fn() },
            addEventListener: (name, listener) => { listeners.set(name, listener) },
            removeEventListener: name => { listeners.delete(name) },
        })
        expect(isLibraryFileOperationReserved()).toBe(true)
        window.dispatchEvent(new Event('focus'))
        document.dispatchEvent(new Event('visibilitychange'))
        await Promise.resolve()
        expect(isLibraryFileOperationReserved()).toBe(true)
        listeners.get('risu-android-content-source-picked')!(new CustomEvent('risu-android-content-source-picked', { detail: { requestId: 'selection', ready: [], failures: [] } }))
        expect(await pending).toBeNull()
        expect(isLibraryFileOperationReserved()).toBe(false)
    })
    it.each(['androidSeekable', 'androidSpool'] as const)('keeps the portable selector custody receipt typed as %s', async (sourceType) => {
        const listeners = new Map<string, (event: Event) => void>()
        const picker = vi.fn((requestId: string) => queueMicrotask(() => listeners.get('risu-android-portable-source-picked')?.(
            new CustomEvent('risu-android-portable-source-picked', {detail: {requestId, ready: [{sourceType, token: '11111111-1111-4111-8111-111111111111', displayName: 'synthetic.risunest', bytes: 4_294_967_296}], failures: []}}),
        )))
        const copy = vi.fn()
        await expect(pickAndroidPortableBackupSource({}, {
            createRequestId: () => 'custody-picker', bridge: {copyExport: copy, pickPortableBackupSource: picker},
            addEventListener: (name, listener) => {listeners.set(name, listener)},
            removeEventListener: name => {listeners.delete(name)},
        })).resolves.toEqual({type: sourceType, token: '11111111-1111-4111-8111-111111111111'})
        expect(copy).not.toHaveBeenCalled()
        expect(picker).toHaveBeenCalledExactlyOnceWith('custody-picker')
        expect(listeners.size).toBe(0)
    })
    it('cancels selected custody with the custody authority and never deletes a spool substitute', async () => {
        const listeners = new Map<string, (event: Event) => void>()
        const controller = new AbortController()
        const discard = vi.fn(() => true)
        const discardSpool = vi.fn(() => true)
        const picked = pickAndroidPortableBackupSource({signal: controller.signal}, {
            createRequestId: () => 'custody-cancel', bridge: {copyExport: vi.fn(), pickPortableBackupSource: vi.fn(), cancelSource: vi.fn(), discardPortableSource: discard, discardSource: discardSpool},
            addEventListener: (name, listener) => {listeners.set(name, listener)},
            removeEventListener: name => {listeners.delete(name)},
        })
        const rejected = expect(picked).rejects.toMatchObject({name: 'AbortError'})
        controller.abort()
        listeners.get('risu-android-portable-source-picked')?.(new CustomEvent('risu-android-portable-source-picked', {detail: {requestId:'custody-cancel', ready:[{sourceType:'androidSeekable', token:'owned-token', displayName:'synthetic.risunest', bytes:42}], failures:[]}}))
        await rejected
        expect(discard).toHaveBeenCalledExactlyOnceWith('owned-token')
        expect(discardSpool).not.toHaveBeenCalled()
    })
    it.each(['complete.RISUNEST', 'compatible.BIN', 'block.RISUDAT'])(
        'picks %s through the common backup picker without transferring bytes to JavaScript',
        async (displayName) => {
            const listeners = new Map<string, Set<(event: Event) => void>>()
            const onSource = vi.fn()
            const onProgress = vi.fn()
            const pickBackupSource = vi.fn((requestId: string) =>
                queueMicrotask(() => {
                    for (const listener of listeners.get(
                        'risu-android-saf-progress',
                    ) ?? []) {
                        listener(
                            new CustomEvent('risu-android-saf-progress', {
                                detail: {
                                    requestId,
                                    operation: 'source-copy',
                                    copiedBytes: 2048,
                                    totalBytes: 4_294_967_296,
                                    token: null,
                                },
                            }),
                        )
                    }
                    for (const listener of listeners.get(
                        'risu-android-backup-source-picked',
                    ) ?? []) {
                        listener(
                            new CustomEvent(
                                'risu-android-backup-source-picked',
                                {
                                    detail: {
                                        requestId,
                                        ready: [
                                            {
                                                token: '11111111-1111-4111-8111-111111111111',
                                                displayName,
                                                bytes: 4_294_967_296,
                                            },
                                        ],
                                        failures: [],
                                    },
                                },
                            ),
                        )
                    }
                }),
            )
            const source = await pickAndroidBackupSource(
                { onSource, onProgress },
                {
                    createRequestId: () => 'common-backup-picker',
                    bridge: { copyExport: vi.fn(), pickBackupSource },
                    addEventListener: (name, listener) => {
                        const registered = listeners.get(name) ?? new Set()
                        registered.add(listener)
                        listeners.set(name, registered)
                    },
                    removeEventListener: (name, listener) =>
                        listeners.get(name)?.delete(listener),
                },
            )
            expect(source).toEqual({
                type: 'androidSpool',
                token: '11111111-1111-4111-8111-111111111111',
            })
            expect(pickBackupSource).toHaveBeenCalledExactlyOnceWith(
                'common-backup-picker',
            )
            expect(onSource).toHaveBeenCalledExactlyOnceWith({ displayName, bytes: 4_294_967_296 })
            expect(onProgress).toHaveBeenCalledExactlyOnceWith(
                expect.objectContaining({ copiedBytes: 2048, totalBytes: 4_294_967_296 }),
            )
            expect([...listeners.values()].every((registered) => registered.size === 0)).toBe(true)
        },
    )

    it.each(['old.risulossless', 'card.charx', 'backup.risunest.exe'])(
        'discards unsupported common-picker source %s',
        async (displayName) => {
            const listeners = new Set<(event: Event) => void>()
            const discardSource = vi.fn(() => true)
            const selected = pickAndroidBackupSource(
                {},
                {
                    createRequestId: () => 'common-backup-rejected',
                    bridge: {
                        copyExport: vi.fn(),
                        discardSource,
                        pickBackupSource: (requestId) =>
                            queueMicrotask(() => {
                                for (const listener of listeners)
                                    listener(
                                        new CustomEvent(
                                            'risu-android-backup-source-picked',
                                            {
                                                detail: {
                                                    requestId,
                                                    ready: [
                                                        {
                                                            token: '11111111-1111-4111-8111-111111111111',
                                                            displayName,
                                                            bytes: 16,
                                                        },
                                                    ],
                                                    failures: [],
                                                },
                                            },
                                        ),
                                    )
                            }),
                    },
                    addEventListener: (_name, listener) =>
                        listeners.add(listener),
                    removeEventListener: (_name, listener) =>
                        listeners.delete(listener),
                },
            )
            await expect(selected).rejects.toMatchObject({ code: 'unsupported-format' })
            expect(discardSource).toHaveBeenCalledExactlyOnceWith(
                '11111111-1111-4111-8111-111111111111',
            )
            expect(listeners.size).toBe(0)
        },
    )

    it('routes all current backup suffixes and excludes previous RisuNest archives from open-with restore', async () => {
        const restore = vi.fn(async (_input: { source: unknown; displayName: string }) => undefined)
        const unsupported = vi.fn()
        const names = ['backup.RISUNEST', 'legacy.BIN', 'block.RISUDAT', 'previous.risulossless']
        await consumeAndroidSpoolBatch(
            {
                requestId: 'common-open-with',
                ready: names.map((displayName, index) => ({
                    token: String(index),
                    displayName,
                    bytes: 10,
                })),
                failures: [],
            },
            { restore, unsupported },
        )
        expect(restore.mock.calls.map(([input]) => input.displayName)).toEqual(names.slice(0, 3))
        expect(unsupported).toHaveBeenCalledExactlyOnceWith({
            token: '3',
            displayName: names[3],
            bytes: 10,
        })
    })

    it('reports SAF file jobs enabled only when the native bridge is installed', () => {
        expect(isAndroidSafFileJobsEnabled(undefined)).toBe(false)
        expect(isAndroidSafFileJobsEnabled({})).toBe(true)
    })

    it('accepts only canonical persisted destination export IDs', async () => {
        expect(await getAndroidSafExportSourceId({
            copyExport: vi.fn(),
            getExportSourceId: async () => '123e4567-e89b-42d3-a456-426614174004',
        })).toBe('123e4567-e89b-42d3-a456-426614174004')
        expect(await getAndroidSafExportSourceId({
            copyExport: vi.fn(),
            getExportSourceId: async () => 'not-owned',
        })).toBeNull()
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

    it('discards a ready source through the token-only native bridge', async () => {
        const discardSource = vi.fn(async () => true)

        expect(await discardAndroidSafSource(
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

    it('receives a lossless picker result as an owned spool token', async () => {
        const listeners = new Map<string, Set<(event: Event) => void>>()
        const progress = vi.fn()
        const pickBackupSource = vi.fn((requestId: string) =>
            queueMicrotask(() => {
                for (const listener of listeners.get(
                    'risu-android-saf-progress',
                ) ?? []) {
                    listener(
                        new CustomEvent('risu-android-saf-progress', {
                            detail: {
                                requestId,
                                operation: 'source-copy',
                                copiedBytes: 5_000,
                                totalBytes: 10_000,
                                token: null,
                            },
                        }),
                    )
                }
                for (const listener of listeners.get(
                    'risu-android-backup-source-picked',
                ) ?? []) {
                    listener(
                        new CustomEvent('risu-android-backup-source-picked', {
                            detail: {
                                requestId,
                                ready: [
                                    {
                                        token: '55555555-5555-4555-8555-555555555555',
                                        displayName: 'chosen.risunest',
                                        bytes: 10_000,
                                    },
                                ],
                                failures: [],
                            },
                        }),
                    )
                }
            }),
        )

        const source = await pickAndroidBackupSource(
            { onProgress: progress },
            {
                createRequestId: () => 'source-picker-1',
                bridge: { copyExport: vi.fn(), pickBackupSource },
                addEventListener: (name, listener) => {
                    const registered = listeners.get(name) ?? new Set()
                    registered.add(listener)
                    listeners.set(name, registered)
                },
                removeEventListener: (name, listener) =>
                    listeners.get(name)?.delete(listener),
            },
        )

        expect(source).toEqual({
            type: 'androidSpool',
            token: '55555555-5555-4555-8555-555555555555',
        })
        expect(pickBackupSource).toHaveBeenCalledExactlyOnceWith(
            'source-picker-1',
        )
        expect(progress).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({
            copiedBytes: 5_000,
            totalBytes: 10_000,
        }))
        expect([...listeners.values()].every((registered) => registered.size === 0)).toBe(true)
    })

    it('treats closing the Android lossless picker as cancellation', async () => {
        const listeners = new Set<(event: Event) => void>()
        const source = await pickAndroidBackupSource(
            {},
            {
                createRequestId: () => 'source-picker-2',
                bridge: {
                    copyExport: vi.fn(),
                    pickBackupSource: () =>
                        queueMicrotask(() => {
                            for (const listener of listeners) {
                                listener(
                                    new CustomEvent(
                                        'risu-android-backup-source-picked',
                                        {
                                            detail: {
                                                requestId: 'source-picker-2',
                                                ready: [],
                                                failures: [],
                                            },
                                        },
                                    ),
                                )
                            }
                        }),
                },
                addEventListener: (_name, listener) => listeners.add(listener),
                removeEventListener: (_name, listener) =>
                    listeners.delete(listener),
            },
        )

        expect(source).toBeNull()
        expect(listeners.size).toBe(0)
    })

    it('cancels the native lossless picker when the shared operation is aborted', async () => {
        const listeners = new Set<(event: Event) => void>()
        const cancelSource = vi.fn()
        const discardSource = vi.fn(() => true)
        const controller = new AbortController()
        const pending = pickAndroidBackupSource(
            { signal: controller.signal },
            {
                createRequestId: () => 'source-picker-3',
                bridge: {
                    copyExport: vi.fn(),
                    pickBackupSource: vi.fn(),
                    cancelSource,
                    discardSource,
                },
                addEventListener: (_name, listener) => listeners.add(listener),
                removeEventListener: (_name, listener) =>
                    listeners.delete(listener),
            },
        )

        controller.abort()

        expect(cancelSource).toHaveBeenCalledExactlyOnceWith('source-picker-3')
        expect(listeners.size).toBe(1)

        for (const listener of [...listeners]) {
            listener(
                new CustomEvent('risu-android-backup-source-picked', {
                    detail: {
                        requestId: 'source-picker-3',
                        ready: [
                            {
                                token: '66666666-6666-4666-8666-666666666666',
                                displayName: 'late.risunest',
                                bytes: 10,
                            },
                        ],
                        failures: [],
                    },
                }),
            )
        }

        await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        expect(discardSource).toHaveBeenCalledExactlyOnceWith(
            '66666666-6666-4666-8666-666666666666',
        )
        expect(listeners.size).toBe(0)
    })

    it('reports cleanup failure when a cancelled picker leaves a ready spool', async () => {
        const listeners = new Set<(event: Event) => void>()
        const controller = new AbortController()
        const pending = pickAndroidBackupSource(
            { signal: controller.signal },
            {
                createRequestId: () => 'source-picker-4',
                bridge: {
                    copyExport: vi.fn(),
                    pickBackupSource: vi.fn(),
                    cancelSource: vi.fn(),
                    discardSource: vi.fn(() => false),
                },
                addEventListener: (_name, listener) => listeners.add(listener),
                removeEventListener: (_name, listener) =>
                    listeners.delete(listener),
            },
        )

        controller.abort()
        for (const listener of [...listeners]) {
            listener(
                new CustomEvent('risu-android-backup-source-picked', {
                    detail: {
                        requestId: 'source-picker-4',
                        ready: [
                            {
                                token: '77777777-7777-4777-8777-777777777777',
                                displayName: 'late.risunest',
                                bytes: 10,
                            },
                        ],
                        failures: [],
                    },
                }),
            )
        }

        await expect(pending).rejects.toMatchObject({ code: 'cleanup-failed' })
        expect(listeners.size).toBe(0)
    })

    it('picks a legacy backup into an owned spool and returns only its token', async () => {
        const listeners = new Set<(event: Event) => void>()
        const pickLegacyBackupSource = vi.fn((requestId: string) => queueMicrotask(() => {
            for (const listener of listeners) {
                listener(new CustomEvent('risu-android-legacy-backup-source-picked', { detail: {
                    requestId,
                    ready: [{
                        token: '11111111-1111-4111-8111-111111111111',
                        displayName: 'backup.bin',
                        bytes: 4_294_967_296,
                        totalBytes: 4_294_967_296,
                    }],
                    failures: [],
                } }))
            }
        }))

        const onSource = vi.fn()
        const source = await pickAndroidLegacyBackupSource({ onSource }, {
            createRequestId: () => 'source-picker-1',
            bridge: { copyExport: vi.fn(), pickLegacyBackupSource },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })

        expect(source).toEqual({
            type: 'androidSpool',
            token: '11111111-1111-4111-8111-111111111111',
        })
        expect(onSource).toHaveBeenCalledExactlyOnceWith({ displayName: 'backup.bin', bytes: 4_294_967_296 })
        expect(pickLegacyBackupSource).toHaveBeenCalledExactlyOnceWith('source-picker-1')
        expect(JSON.stringify(source)).not.toContain('Uint8Array')
        expect(listeners.size).toBe(0)
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
            sourcePath: '/data/user/0/io.github.rsyumi.risunest/files/persistent/exports/risusave-a.risudat',
            suggestedName: 'backup.risudat',
        }, {
            createRequestId: () => 'request-1',
            bridge: { copyExport, acknowledgeExport },
            addEventListener: (_name, listener) => listeners.add(listener),
            removeEventListener: (_name, listener) => listeners.delete(listener),
        })

        expect(copyExport).toHaveBeenCalledExactlyOnceWith(
            'request-1',
            '/data/user/0/io.github.rsyumi.risunest/files/persistent/exports/risusave-a.risudat',
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
            sourcePath: '/data/user/0/io.github.rsyumi.risunest/files/persistent/exports/risusave-a.risudat',
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
            sourcePath: '/data/user/0/io.github.rsyumi.risunest/persistent/exports/risusave-a.risudat',
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
            sourcePath: '/data/user/0/io.github.rsyumi.risunest/native-file-jobs/screenshot-output/11111111-1111-4111-8111-111111111111/archive.zip.part',
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
            sourcePath: '/data/user/0/io.github.rsyumi.risunest/persistent/exports/risusave-a.risudat',
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
            sourcePath: '/data/user/0/io.github.rsyumi.risunest/persistent/exports/risusave-a.risudat',
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

    it('rejects a dismissed picker with AbortError where DOMException.code has only a getter', async () => {
        // WebViews define code as a getter without a setter; the test DOM does not.
        const original = Object.getOwnPropertyDescriptor(DOMException.prototype, 'code')
        Object.defineProperty(DOMException.prototype, 'code', { get: () => 20, configurable: true })
        try {
            const listeners = new Set<(event: Event) => void>()
            const promise = copyNativeExportToAndroidSaf({
                sourcePath: '/data/user/0/io.github.rsyumi.risunest/files/native-file-jobs/handoffs/risu-dataset-11111111-1111-4111-8111-111111111111.json',
                suggestedName: 'dataset.json',
            }, {
                createRequestId: () => 'request-6',
                bridge: { copyExport: vi.fn() },
                addEventListener: (_name, listener) => listeners.add(listener),
                removeEventListener: (_name, listener) => listeners.delete(listener),
            })
            for (const listener of listeners) {
                listener(new CustomEvent('risu-android-saf-destination', { detail: {
                    requestId: 'request-6',
                    exportId: '11111111-1111-4111-8111-111111111111',
                    sourceKind: 'risuSave',
                    state: 'cancelled',
                    code: 'cancelled',
                    warningCodes: [],
                } satisfies AndroidSafDestinationEvent }))
            }

            await expect(promise).rejects.toMatchObject({
                name: 'AbortError',
                requestId: 'request-6',
                code: 'cancelled',
                warningCodes: [],
            })
            expect(listeners.size).toBe(0)
        } finally {
            if (original) Object.defineProperty(DOMException.prototype, 'code', original)
            else delete (DOMException.prototype as { code?: unknown }).code
        }
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
            sourcePath: '/data/user/0/io.github.rsyumi.risunest/files/persistent/exports/source.risudat',
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

function contentPickerDependencies(detail: { ready: unknown[]; failures: unknown[] }, discardSource = vi.fn(() => true)) {
    const listeners = new Set<(event: Event) => void>()
    const destinations: string[] = []
    return {
        listeners,
        destinations,
        discardSource,
        dependencies: {
            createRequestId: () => 'request',
            bridge: {
                copyExport: vi.fn(),
                discardSource,
                pickContentSource: (requestId: string, destination: string) => {
                    destinations.push(destination)
                    queueMicrotask(() => {
                        for (const listener of listeners) {
                            listener(new CustomEvent('risu-android-content-source-picked', { detail: { requestId, ...detail } }))
                        }
                    })
                },
            },
            addEventListener: (_name: string, listener: (event: Event) => void) => { listeners.add(listener) },
            removeEventListener: (_name: string, listener: (event: Event) => void) => { listeners.delete(listener) },
        },
    }
}

it('selects several content spools without passing bytes through the WebView and refuses other files', async () => {
    const large = {
        token: '11111111-1111-4111-8111-111111111111',
        displayName: 'large.CHARX',
        bytes: 500 * 1024 * 1024,
        importDestination: 'module',
    }
    const card = { token: '22222222-2222-4222-8222-222222222222', displayName: 'card.png', bytes: 64, importDestination: 'module' }
    const notes = { token: '33333333-3333-4333-8333-333333333333', displayName: 'notes.txt', bytes: 8, importDestination: 'module' }
    const picker = contentPickerDependencies({
        ready: [large, notes, card],
        failures: [{ displayName: 'program.exe', code: 'unsupported-source' }],
    })

    await expect(pickAndroidContentSources({ destination: 'module' }, picker.dependencies)).resolves.toEqual({
        sources: [large, card],
        failures: [
            { displayName: 'program.exe', code: 'unsupported-source' },
            { displayName: 'notes.txt', code: 'unsupported-format' },
        ],
    })
    expect(picker.destinations).toEqual(['module'])
    expect(picker.discardSource).toHaveBeenCalledExactlyOnceWith(notes.token)
    expect(picker.listeners.size).toBe(0)
})

it('rejects a single refused file and lists several refused files together', async () => {
    const single = contentPickerDependencies({ ready: [], failures: [{ displayName: 'program.exe', code: 'unsupported-source' }] })
    await expect(pickAndroidContentSources({ destination: 'character' }, single.dependencies)).rejects.toMatchObject({
        code: 'unsupported-source',
        message: 'program.exe: unsupported-source',
    })
    expect(single.listeners.size).toBe(0)

    const failures = [
        { displayName: 'program.exe', code: 'unsupported-source' },
        { displayName: 'card.png', code: 'storage-full' },
    ]
    const several = contentPickerDependencies({ ready: [], failures })
    await expect(pickAndroidContentSources({ destination: 'character' }, several.dependencies)).resolves.toEqual({ sources: [], failures })
    expect(several.listeners.size).toBe(0)
})
