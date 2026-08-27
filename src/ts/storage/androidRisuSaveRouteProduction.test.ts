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
        const ready = ['card.json', 'card.charx', 'card.jpg', 'card.JPEG'].map((displayName, index) => ({
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
})
