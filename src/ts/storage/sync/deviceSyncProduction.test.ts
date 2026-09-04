import { describe, expect, it, vi } from 'vitest'

vi.mock('../persistentDataRuntime.svelte', () => ({
    flushPendingData: vi.fn(),
    capturePersistentMutationToken: vi.fn(),
    acquireDestructiveReplacementFence: vi.fn(),
}))

import {
    createAndroidDeviceSyncCloneTarget,
    createProductionDeviceSyncController,
} from './deviceSyncProduction'
import { createDeviceSyncController, startDeviceSyncAutoListen } from './deviceSyncController'

describe('production device sync composition', () => {
    it('selects the Android unified source facade while leaving desktop composition injectable', () => {
        const runtime = {
            flushPendingData: vi.fn(), capturePersistentMutationToken: vi.fn(),
            acquireDestructiveReplacementFence: vi.fn(),
        }
        const androidFacade = { platform: 'android-source' }
        const sourceAndroid = vi.fn(() => androidFacade)
        const sourceDesktop = vi.fn(() => ({ platform: 'desktop-source' }))
        const createController = vi.fn((_options: unknown) => ({ kind: 'controller' }))

        createProductionDeviceSyncController({
            platform: 'android', runtime,
            factories: {
                sourceAndroid: sourceAndroid as never,
                sourceDesktop: sourceDesktop as never,
                cloneDesktop: vi.fn(),
                deltaDesktop: vi.fn(),
                cloneAndroid: vi.fn(() => ({ kind: 'clone' }) as never),
                deltaAndroid: vi.fn(() => ({ kind: 'delta' }) as never),
                bidirectional: vi.fn(() => ({ kind: 'bidirectional' }) as never),
                controller: createController as never,
            },
        })

        expect(sourceAndroid).toHaveBeenCalledOnce()
        expect(sourceAndroid).toHaveBeenCalledWith(runtime)
        expect(sourceDesktop).not.toHaveBeenCalled()
        expect(createController.mock.calls[0]?.[0]).toMatchObject({ facade: androidFacade })
    })

    it('composes every desktop target before returning the controller', () => {
        const runtime = {
            flushPendingData: vi.fn(), capturePersistentMutationToken: vi.fn(),
            acquireDestructiveReplacementFence: vi.fn(),
        }
        const clone = { kind: 'clone' }
        const delta = { kind: 'delta' }
        const bidirectional = { kind: 'bidirectional' }
        const createController = vi.fn((_options: {
            facade: unknown
            targets: { clone: unknown; delta: unknown; bidirectional: unknown }
        }) => ({ kind: 'controller' }))
        const facade = { kind: 'facade' }

        const controller = createProductionDeviceSyncController({
            platform: 'desktop', runtime, facade: facade as never,
            factories: {
                cloneDesktop: vi.fn(() => clone as never),
                deltaDesktop: vi.fn(() => delta as never),
                deltaAndroid: vi.fn(),
                bidirectional: vi.fn(() => bidirectional as never),
                cloneAndroid: vi.fn(),
                controller: createController as never,
            },
        })

        expect(controller).toEqual({ kind: 'controller' })
        const composition = createController.mock.calls[0]![0]
        expect(composition.facade).toBe(facade)
        expect(composition.targets.clone).toMatchObject(clone)
        expect(composition.targets.delta).toMatchObject(delta)
        expect(composition.targets.bidirectional).toMatchObject(bidirectional)
    })

    it('rejects swallowed desktop target initialization failures until recovery succeeds', async () => {
        let cloneError = 'private native initialization failure'
        const initializeClone = vi.fn(async () => undefined)
        const clone = {
            snapshot: () => ({
                state: { target: { phase: 'idle' as const, destructiveConfirmed: false, completedBytes: 0 } },
                error: cloneError, warning: '',
            }),
            subscribe: () => () => undefined,
            initialize: initializeClone,
        }
        const delta = {
            snapshot: () => ({ pullPhase: 'idle' as const, retained: null, error: '' }),
            subscribe: () => () => undefined,
            initialize: vi.fn(async () => undefined),
        }
        const bidirectional = {
            snapshot: () => ({
                operationPhase: 'idle' as const, operationRetained: false, operationError: '',
            }),
            subscribe: () => () => undefined,
            initialize: vi.fn(async () => undefined),
        }
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
        const start = vi.fn(async () => ({ phase: 'running' as const }))
        const facade = {
            status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
            prepare, start, stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
            revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
        }
        const controller = createProductionDeviceSyncController({
            platform: 'desktop', facade: facade as never,
            factories: {
                cloneDesktop: () => clone as never, cloneAndroid: vi.fn(),
                deltaDesktop: () => delta as never, deltaAndroid: vi.fn(),
                bidirectional: () => bidirectional as never,
                controller: createDeviceSyncController,
            },
        })
        const report = vi.fn()

        await expect(controller.initialize()).rejects.toMatchObject({ code: 'state-unavailable' })
        await startDeviceSyncAutoListen({
            syncAutoListen: true, syncListenMethod: 'lan', syncFixedPort: 32145, syncPublicBaseUrl: '',
        }, { controller, report })
        expect(prepare).not.toHaveBeenCalled()
        expect(start).not.toHaveBeenCalled()
        expect(report).toHaveBeenCalledWith(expect.objectContaining({ code: 'state-unavailable' }))

        cloneError = ''
        await expect(controller.initialize()).resolves.toBeUndefined()
        expect(initializeClone).toHaveBeenCalledTimes(3)
        await expect(controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .resolves.toMatchObject({ phase: 'prepared' })
        expect(prepare).toHaveBeenCalledOnce()
    })

    it('adapts the Android registered clone job to the unified target API', async () => {
        const listeners: Array<() => void> = []
        let state: {
            phase: 'idle' | 'paused' | 'downloading' | 'cancelled' | 'completed'
            destructiveConfirmed: boolean
            activationCommitted: boolean
            completedBytes: number
            backupPaths?: string[]
        } = {
            phase: 'idle', destructiveConfirmed: false, activationCommitted: false,
            completedBytes: 0,
        }
        const facade = {
            getState: () => state,
            capabilities: vi.fn(async () => ({
                androidClient: true, atomicActivationReady: true, losslessBackupReady: true,
                httpTransportReady: true, productionEnabled: true,
            })),
            recover: vi.fn(async () => null),
            joinRegistered: vi.fn(async () => {
                state = { ...state, phase: 'paused', destructiveConfirmed: false }
                return state
            }),
            confirmDestructiveReplace: vi.fn(() => {
                state = { ...state, destructiveConfirmed: true }
                return state
            }),
            download: vi.fn(async () => { state = { ...state, phase: 'downloading' } }),
            resume: vi.fn(async () => undefined),
            cancel: vi.fn(async () => { state = { ...state, phase: 'cancelled' } }),
            targetStatus: vi.fn(async () => ({
                sourceDeviceId: 'source', jobId: 'job',
                phase: 'downloading' as const, completedBytes: 1,
            })),
        }
        const target = createAndroidDeviceSyncCloneTarget(facade, {
            schedule: (listener) => { listeners.push(listener); return 1 },
            cancelSchedule: vi.fn(),
        })

        await target.initialize()
        await target.joinRegistered('source')
        expect(target.snapshot().state.target.phase).toBe('joined')
        expect(target.snapshot().resumeAvailable).toBe(true)
        target.confirmDestructiveReplace()
        await target.download()
        expect(facade.joinRegistered).toHaveBeenCalledWith('source')
        expect(facade.download).toHaveBeenCalledOnce()
        expect(listeners).toHaveLength(1)
        state = {
            ...state,
            phase: 'completed',
            backupPaths: ['/data/user/0/app/pre-clone.lossless'],
        }
        listeners[0]()
        expect(target.snapshot().state.target.backupPaths)
            .toEqual(['/data/user/0/app/pre-clone.lossless'])
        target.dispose()
    })
})
