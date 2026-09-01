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

describe('production device sync composition', () => {
    it('composes every desktop target before returning the controller', () => {
        const runtime = {
            flushPendingData: vi.fn(), capturePersistentMutationToken: vi.fn(),
            acquireDestructiveReplacementFence: vi.fn(),
        }
        const clone = { kind: 'clone' }
        const delta = { kind: 'delta' }
        const bidirectional = { kind: 'bidirectional' }
        const createController = vi.fn(() => ({ kind: 'controller' }))
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
        expect(createController).toHaveBeenCalledWith({
            facade,
            targets: { clone, delta, bidirectional },
        })
    })

    it('adapts the Android registered clone job to the unified target API', async () => {
        const listeners: Array<() => void> = []
        let state: {
            phase: 'idle' | 'paused' | 'downloading' | 'cancelled'
            destructiveConfirmed: boolean
            activationCommitted: boolean
            completedBytes: number
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
                jobId: 'job', endpoint: 'http://source/', sessionId: 'session', manifestId: 'manifest',
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
        target.dispose()
    })
})
