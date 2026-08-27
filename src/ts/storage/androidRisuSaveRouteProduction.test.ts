import { describe, expect, it, vi } from 'vitest'

vi.mock('src/lang', () => ({ language: {} }))
vi.mock('../alert', () => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
}))
vi.mock('../platform', () => ({ isTauriAndroid: false }))
vi.mock('../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: vi.fn(),
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: vi.fn(),
}))

import {
    createAndroidOpenedSpoolDispatcher,
    dispatchAndroidOpenedSpoolBatch,
    type AndroidOpenedSpoolDispatchDependencies,
} from './androidRisuSaveRouteProduction.svelte'

describe('Android opened spool production route', () => {
    it('keeps the inactive content capability fallback out of the restore discard path', async () => {
        const enqueueRestore = vi.fn(async () => undefined)
        const importCharacter = vi.fn(async () => ({
            kind: 'capability-unavailable' as const,
        }))
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore,
            importCharacter,
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const character = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.charx',
            bytes: 20,
        }
        const save = {
            token: '22222222-2222-4222-8222-222222222222',
            displayName: 'backup.risudat',
            bytes: 30,
        }
        const unsupported = {
            token: '33333333-3333-4333-8333-333333333333',
            displayName: 'portrait.png',
            bytes: 40,
        }

        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-1',
            ready: [character, save, unsupported],
            failures: [],
        }, dependencies)

        expect(importCharacter).toHaveBeenCalledExactlyOnceWith(character)
        expect(enqueueRestore).toHaveBeenCalledExactlyOnceWith({
            requestId: 'opened-1',
            ready: [save, unsupported],
            failures: [],
        })
        expect(dependencies.reportCharacterError).not.toHaveBeenCalled()
        expect(dependencies.reportDestinationRequired).not.toHaveBeenCalled()
    })

    it('sends every recognized Android content extension through the native character caller', async () => {
        const enqueueRestore = vi.fn(async () => undefined)
        const importCharacter = vi.fn(async () => ({ kind: 'declined' as const }))
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore,
            importCharacter,
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const ready = [
            `${'a'.repeat(174)}.charx`,
            'card.json',
            'card.jpg',
            'card.JPEG',
        ].map((displayName, index) => ({
            token: `00000000-0000-4000-8000-00000000000${index + 1}`,
            displayName,
            bytes: 10,
        }))

        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-2',
            ready,
            failures: [],
        }, dependencies)

        expect(importCharacter).toHaveBeenCalledTimes(4)
        expect(importCharacter).toHaveBeenNthCalledWith(1, ready[0])
        expect(importCharacter).toHaveBeenNthCalledWith(2, ready[1])
        expect(importCharacter).toHaveBeenNthCalledWith(3, ready[2])
        expect(importCharacter).toHaveBeenNthCalledWith(4, ready[3])
        expect(enqueueRestore).toHaveBeenCalledExactlyOnceWith({
            requestId: 'opened-2',
            ready: [],
            failures: [],
        })
    })

    it('reports the explicit destination result for an ordinary Android JPEG', async () => {
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore: vi.fn(async () => undefined),
            importCharacter: vi.fn(async () => ({ kind: 'destination-required' as const })),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const source = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'portrait.jpeg',
            bytes: 10,
        }

        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-jpeg',
            ready: [source],
            failures: [],
        }, dependencies)

        expect(dependencies.reportDestinationRequired).toHaveBeenCalledExactlyOnceWith(source)
        expect(dependencies.reportCharacterError).not.toHaveBeenCalled()
    })

    it('deduplicates a replayed character token for one dispatcher lifetime', async () => {
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore: vi.fn(async () => undefined),
            importCharacter: vi.fn(async () => ({ kind: 'declined' as const })),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const dispatcher = createAndroidOpenedSpoolDispatcher(dependencies)
        const source = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.charx',
            bytes: 10,
        }

        await dispatcher.enqueue({ requestId: 'opened-1', ready: [source], failures: [] })
        await dispatcher.enqueue({ requestId: 'replayed-1', ready: [source], failures: [] })

        expect(dependencies.importCharacter).toHaveBeenCalledExactlyOnceWith(source)
    })

    it('keeps distinct character tokens with the same name in FIFO order', async () => {
        let releaseFirst!: () => void
        const calls: string[] = []
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore: vi.fn(async () => undefined),
            importCharacter: vi.fn(async (source) => {
                calls.push(source.token)
                if (calls.length === 1) {
                    await new Promise<void>((resolve) => releaseFirst = resolve)
                }
                return { kind: 'declined' as const }
            }),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const dispatcher = createAndroidOpenedSpoolDispatcher(dependencies)
        const first = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.charx',
            bytes: 10,
        }
        const second = {
            token: '22222222-2222-4222-8222-222222222222',
            displayName: 'card.charx',
            bytes: 10,
        }

        const firstBatch = dispatcher.enqueue({
            requestId: 'opened-1',
            ready: [first],
            failures: [],
        })
        await vi.waitFor(() => expect(calls).toEqual([first.token]))
        const secondBatch = dispatcher.enqueue({
            requestId: 'opened-2',
            ready: [second],
            failures: [],
        })
        await Promise.resolve()
        expect(calls).toEqual([first.token])

        releaseFirst()
        await Promise.all([firstBatch, secondBatch])
        expect(calls).toEqual([first.token, second.token])
    })
})
