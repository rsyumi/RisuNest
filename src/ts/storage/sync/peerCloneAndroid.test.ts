import { describe, expect, it, vi } from 'vitest'

import {
    createAndroidPeerCloneFacade,
    type AndroidPeerCloneBridge,
    type AndroidPeerCloneInvoke,
    type AndroidPeerCloneReplacementRuntime,
} from './peerCloneAndroid'

const pairingUri = 'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-42d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'

function runtime() {
    const release = vi.fn()
    const refreshCommittedWorkingSet = vi.fn(async (_revision: number) => {})
    const replacement: AndroidPeerCloneReplacementRuntime = {
        capturePersistentMutationToken: vi.fn(async () => ({ revision: 7, mutationGeneration: 11 })),
        acquireDestructiveReplacementFence: vi.fn(async () => ({ release, refreshCommittedWorkingSet })),
    }
    return { replacement, release, refreshCommittedWorkingSet }
}

function bridge(mode: 'foreground' | 'uidt' = 'uidt') {
    const value: AndroidPeerCloneBridge = {
        transferMode: vi.fn(() => mode),
        schedule: vi.fn(() => 'scheduled' as const),
        cancel: vi.fn(() => true),
    }
    return value
}

describe('Android peer clone facade', () => {
    it('claims once and schedules API 34 UIDT with only the opaque native job id', async () => {
        const calls: [string, Record<string, unknown> | undefined][] = []
        const invoke: AndroidPeerCloneInvoke = async <T>(command: string, args?: Record<string, unknown>) => {
            calls.push([command, args])
            if (command === 'peer_clone_android_capabilities') {
                return {
                    androidClient: true,
                    atomicActivationReady: true,
                    losslessBackupReady: true,
                    httpTransportReady: true,
                    productionEnabled: true,
                } as T
            }
            if (command === 'peer_clone_android_claim') {
                return { jobId: '11111111-1111-4111-8111-111111111111', phase: 'ready' } as T
            }
            throw new Error(`unexpected command ${command}`)
        }
        const nativeBridge = bridge('uidt')
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: nativeBridge,
            runtime: runtime().replacement,
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await facade.download()

        expect(calls).toEqual([
            ['peer_clone_android_capabilities', undefined],
            ['peer_clone_android_claim', {
                endpoint: 'http://192.168.1.4:43123/',
                sessionId: '123e4567-e89b-42d3-a456-426614174000',
                manifestId: 'a'.repeat(64),
                claim: 'b'.repeat(64),
            }],
        ])
        expect(nativeBridge.schedule).toHaveBeenCalledWith('11111111-1111-4111-8111-111111111111')
        expect(JSON.stringify(nativeBridge)).not.toContain('bbbbbbbb')
    })

    it('runs the same native job in foreground below API 34 without scheduling background work', async () => {
        let finishForeground: (() => void) | undefined
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_capabilities') {
                return {
                    androidClient: true,
                    atomicActivationReady: true,
                    losslessBackupReady: true,
                    httpTransportReady: true,
                    productionEnabled: true,
                } as T
            }
            if (command === 'peer_clone_android_claim') {
                return { jobId: '11111111-1111-4111-8111-111111111111', phase: 'ready' } as T
            }
            if (command === 'peer_clone_android_download') {
                return new Promise<void>((resolve) => { finishForeground = resolve }) as Promise<T>
            }
            throw new Error(`unexpected command ${command}`)
        })
        const nativeBridge = bridge('foreground')
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: nativeBridge,
            runtime: runtime().replacement,
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await facade.download()

        expect(nativeBridge.schedule).not.toHaveBeenCalled()
        expect(invoke).toHaveBeenCalledWith('peer_clone_android_download', {
            jobId: '11111111-1111-4111-8111-111111111111',
        })
        finishForeground?.()
    })

    it('recovers the persisted job and finalizes only under the destructive replacement fence', async () => {
        const order: string[] = []
        const { replacement, release, refreshCommittedWorkingSet } = runtime()
        replacement.capturePersistentMutationToken = vi.fn(async () => {
            order.push('token')
            return { revision: 7, mutationGeneration: 11 }
        })
        replacement.acquireDestructiveReplacementFence = vi.fn(async () => {
            order.push('fence')
            return {
                refreshCommittedWorkingSet: async (revision) => {
                    order.push(`refresh:${revision}`)
                    await refreshCommittedWorkingSet(revision)
                },
                release: () => {
                    order.push('release')
                    release()
                },
            }
        })
        const invoke = vi.fn(async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'awaitingActivation',
                    completedBytes: 42,
                    totalBytes: 42,
                } as T
            }
            if (command === 'peer_clone_android_finalize') {
                order.push(`finalize:${args?.expectedRevision}`)
                return { revision: 8 } as T
            }
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })

        await facade.recover()
        const status = await facade.targetStatus()

        expect(status.phase).toBe('completed')
        expect(order).toEqual(['token', 'fence', 'finalize:7', 'refresh:8', 'release'])
        expect(invoke).toHaveBeenCalledWith('peer_clone_android_finalize', {
            jobId: '11111111-1111-4111-8111-111111111111',
            expectedRevision: 7,
        })
    })

    it('persists cancellation before asking the platform job to stop', async () => {
        const order: string[] = []
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'downloading',
                    completedBytes: 1,
                } as T
            }
            if (command === 'peer_clone_android_request_cancel') {
                order.push('marker')
                return undefined as T
            }
            throw new Error(`unexpected command ${command}`)
        })
        const nativeBridge = bridge()
        nativeBridge.cancel = vi.fn(() => {
            order.push('platform')
            return true
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: nativeBridge,
            runtime: runtime().replacement,
        })

        await facade.recover()
        await facade.cancel()

        expect(order).toEqual(['marker', 'platform'])
    })

    it('fails closed when native lossless activation gates are unavailable', async () => {
        const invoke = vi.fn(async <T>(): Promise<T> => ({
            androidClient: true,
            atomicActivationReady: false,
            losslessBackupReady: true,
            httpTransportReady: true,
            productionEnabled: false,
        }) as T)
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: runtime().replacement,
        })

        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        await expect(facade.download()).rejects.toThrow('Android peer clone is not enabled by native production gates')
    })
})
