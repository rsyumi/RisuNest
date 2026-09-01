import { describe, expect, it, vi } from 'vitest'

import {
    createAndroidPeerCloneFacade,
    getAndroidPeerCloneFacade,
    type AndroidPeerCloneBridge,
    type AndroidPeerCloneInvoke,
    type AndroidPeerCloneReplacementRuntime,
} from './peerCloneAndroid'
import { NativeFileJobActivationCommittedError } from '../nativeFileJobs'

const pairingUri = 'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-42d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'

function runtime() {
    const release = vi.fn()
    const refreshCommittedWorkingSet = vi.fn(async (_revision: number) => {})
    const replacement: AndroidPeerCloneReplacementRuntime = {
        capturePersistentMutationToken: vi.fn(async () => ({ revision: 7, mutationGeneration: 11 })),
        acquireDestructiveReplacementFence: vi.fn(async () => ({ release, refreshCommittedWorkingSet })),
        afterRefresh: vi.fn(async () => {}),
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
    it('caches one module-level facade so retained recovery fences survive settings remounts', () => {
        const first = getAndroidPeerCloneFacade(runtime().replacement)
        const second = getAndroidPeerCloneFacade(runtime().replacement)

        expect(second).toBe(first)
    })

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

    it('creates a registered job and starts it without reusing the staged claim', async () => {
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_capabilities') {
                return {
                    androidClient: true, atomicActivationReady: true, losslessBackupReady: true,
                    httpTransportReady: true, productionEnabled: true,
                } as T
            }
            if (command === 'peer_clone_claim_registered_client') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'ready',
                    completedBytes: 0,
                } as T
            }
            throw new Error(`unexpected command ${command}`)
        })
        const nativeBridge = bridge('uidt')
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: nativeBridge,
            runtime: runtime().replacement,
        })

        await facade.joinRegistered('source-device')
        expect(facade.getState().destructiveConfirmed).toBe(false)
        await expect(facade.download()).rejects.toThrow('destructive replacement confirmation')
        facade.confirmDestructiveReplace()
        await facade.download()

        expect(invoke).toHaveBeenCalledWith('peer_clone_claim_registered_client', { deviceId: 'source-device' })
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_android_claim')).toBe(false)
        expect(nativeBridge.schedule).toHaveBeenCalledWith('11111111-1111-4111-8111-111111111111')
    })

    it('captures the confirmed pairing before the capability await', async () => {
        type Capabilities = {
            androidClient: boolean
            atomicActivationReady: boolean
            losslessBackupReady: boolean
            httpTransportReady: boolean
            productionEnabled: boolean
        }
        let resolveCapabilities: ((value: Capabilities) => void) | undefined
        const capabilityPromise = new Promise<Capabilities>((resolve) => { resolveCapabilities = resolve })
        const invoke = vi.fn(<T>(command: string, args?: Record<string, unknown>): Promise<T> => {
            if (command === 'peer_clone_android_capabilities') {
                return capabilityPromise as Promise<T>
            }
            if (command === 'peer_clone_android_claim') {
                return Promise.resolve({
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: args?.endpoint,
                    sessionId: args?.sessionId,
                    manifestId: args?.manifestId,
                    phase: 'ready',
                    completedBytes: 0,
                } as T)
            }
            return Promise.reject(new Error(`unexpected command ${command}`))
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge('uidt'),
            runtime: runtime().replacement,
        })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()

        const download = facade.download()
        await vi.waitFor(() => expect(resolveCapabilities).toBeTypeOf('function'))
        facade.join(
            'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.5%3A43123&session=223e4567-e89b-42d3-a456-426614174000&manifest=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc#claim=dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd',
        )
        resolveCapabilities?.({
            androidClient: true,
            atomicActivationReady: true,
            losslessBackupReady: true,
            httpTransportReady: true,
            productionEnabled: true,
        })
        await download

        expect(invoke).toHaveBeenCalledWith('peer_clone_android_claim', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-42d3-a456-426614174000',
            manifestId: 'a'.repeat(64),
            claim: 'b'.repeat(64),
        })
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
        replacement.afterRefresh = vi.fn(async () => {
            order.push('plugins')
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
            if (command === 'peer_clone_android_release') {
                order.push('native-release')
                return undefined as T
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
        expect(order).toEqual(['token', 'fence', 'finalize:7', 'refresh:8', 'plugins', 'native-release', 'release'])
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

    it('recovers an interrupted foreground transfer as resumable instead of active', async () => {
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'ready',
                    completedBytes: 1,
                    totalBytes: 42,
                } as T
            }
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge('foreground'),
            runtime: runtime().replacement,
        })

        await facade.recover()

        expect(facade.getState()).toMatchObject({
            phase: 'paused',
            destructiveConfirmed: false,
            completedBytes: 1,
            totalBytes: 42,
        })
        await expect(facade.resume()).rejects.toThrow('destructive replacement confirmation')
    })

    it('clears a pre-commit finalization failure so activation can retry', async () => {
        const { replacement } = runtime()
        replacement.capturePersistentMutationToken = vi.fn()
            .mockRejectedValueOnce(new Error('temporary fence failure'))
            .mockResolvedValue({ revision: 7, mutationGeneration: 11 })
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
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
            if (command === 'peer_clone_android_finalize') return { revision: 8 } as T
            if (command === 'peer_clone_android_release') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })

        await facade.recover()
        await expect(facade.targetStatus()).rejects.toThrow('temporary fence failure')
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(replacement.capturePersistentMutationToken).toHaveBeenCalledTimes(2)
    })

    it('retains committed activation while plugin reload is retried under the same fence', async () => {
        const { replacement, release, refreshCommittedWorkingSet } = runtime()
        replacement.afterRefresh = vi.fn()
            .mockRejectedValueOnce(new Error('plugin reload failed'))
            .mockResolvedValue(undefined)
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
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
            if (command === 'peer_clone_android_finalize') return { revision: 8 } as T
            if (command === 'peer_clone_android_release') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })

        await facade.recover()
        const failure = await facade.targetStatus().catch((error: unknown) => error)

        expect(failure).toBeInstanceOf(NativeFileJobActivationCommittedError)
        expect(failure).toMatchObject({ committedRevision: 8, recoveryRequired: true })
        expect(facade.getState().phase).toBe('downloading')
        expect(release).not.toHaveBeenCalled()
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_release')).toHaveLength(1)
        expect(refreshCommittedWorkingSet).toHaveBeenCalledTimes(1)
        expect(replacement.afterRefresh).toHaveBeenCalledTimes(2)
        expect(release).toHaveBeenCalledOnce()
    })

    it('retains the destructive fence and retries only renderer recovery after native commit', async () => {
        const { replacement, release, refreshCommittedWorkingSet } = runtime()
        let currentCalls = 0
        refreshCommittedWorkingSet
            .mockRejectedValueOnce(new Error('temporary renderer refresh failure'))
            .mockResolvedValue(undefined)
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                currentCalls += 1
                if (currentCalls > 2) throw new Error('transient status IPC failure')
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
            if (command === 'peer_clone_android_finalize') return { revision: 8 } as T
            if (command === 'peer_clone_android_release') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })

        await facade.recover()
        const failure = await facade.targetStatus().catch((error: unknown) => error)

        expect(failure).toBeInstanceOf(NativeFileJobActivationCommittedError)
        expect(facade.getState().phase).toBe('downloading')
        expect(release).not.toHaveBeenCalled()
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_release')).toHaveLength(1)
        expect(replacement.capturePersistentMutationToken).toHaveBeenCalledTimes(1)
        expect(replacement.acquireDestructiveReplacementFence).toHaveBeenCalledTimes(1)
        expect(refreshCommittedWorkingSet).toHaveBeenCalledTimes(2)
        expect(currentCalls).toBe(2)
        expect(release).toHaveBeenCalledOnce()
    })

    it('serializes concurrent polling while committed renderer recovery is pending', async () => {
        const { replacement, release, refreshCommittedWorkingSet } = runtime()
        let resolveRefresh: (() => void) | undefined
        refreshCommittedWorkingSet.mockImplementation(() => new Promise<void>((resolve) => {
            resolveRefresh = resolve
        }))
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
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
            if (command === 'peer_clone_android_finalize') return { revision: 8 } as T
            if (command === 'peer_clone_android_release') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })
        await facade.recover()

        const first = facade.targetStatus()
        await vi.waitFor(() => expect(refreshCommittedWorkingSet).toHaveBeenCalledOnce())
        const second = facade.targetStatus()

        expect(refreshCommittedWorkingSet).toHaveBeenCalledOnce()
        resolveRefresh?.()
        await expect(Promise.all([first, second])).resolves.toEqual([
            expect.objectContaining({ phase: 'completed' }),
            expect.objectContaining({ phase: 'completed' }),
        ])
        expect(replacement.afterRefresh).toHaveBeenCalledOnce()
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_release')).toHaveLength(1)
        expect(release).toHaveBeenCalledOnce()
    })

    it('recovers a durably committed activation without invoking native finalize again', async () => {
        const { replacement, release, refreshCommittedWorkingSet } = runtime()
        replacement.capturePersistentMutationToken = vi.fn(async () => ({
            revision: 8,
            mutationGeneration: 12,
        }))
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'awaitingActivation',
                    completedBytes: 42,
                    totalBytes: 42,
                    committedRevision: 8,
                } as T
            }
            if (command === 'peer_clone_android_release') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })

        await facade.recover()
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })

        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_android_finalize')).toBe(false)
        expect(invoke).toHaveBeenCalledWith('peer_clone_android_release', {
            jobId: '11111111-1111-4111-8111-111111111111',
        })
        expect(refreshCommittedWorkingSet).toHaveBeenCalledWith(8)
        expect(replacement.capturePersistentMutationToken).toHaveBeenCalledWith('peer-clone-target-finalize')
        expect(release).toHaveBeenCalledOnce()
    })

    it('rejects cancellation before native request once activation is committed', async () => {
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'awaitingActivation',
                    completedBytes: 42,
                    totalBytes: 42,
                    committedRevision: 8,
                } as T
            }
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: runtime().replacement,
        })

        await facade.recover()

        expect(facade.getState().activationCommitted).toBe(true)
        await expect(facade.cancel()).rejects.toThrow('activation is already committed')
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_android_request_cancel')).toBe(false)
    })

    it('refreshes a restarted committed activation at the newer captured revision', async () => {
        const { replacement, refreshCommittedWorkingSet } = runtime()
        replacement.capturePersistentMutationToken = vi.fn(async () => ({
            revision: 12,
            mutationGeneration: 19,
        }))
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'awaitingActivation',
                    completedBytes: 42,
                    totalBytes: 42,
                    committedRevision: 8,
                } as T
            }
            if (command === 'peer_clone_android_release') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })

        await facade.recover()
        await facade.targetStatus()

        expect(refreshCommittedWorkingSet).toHaveBeenCalledWith(12)
    })

    it('retries only native release when renderer recovery already succeeded', async () => {
        const { replacement, release, refreshCommittedWorkingSet } = runtime()
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
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
            if (command === 'peer_clone_android_finalize') return { revision: 8 } as T
            if (command === 'peer_clone_android_release') {
                if (invoke.mock.calls.filter(([called]) => called === command).length === 1) {
                    throw new Error('temporary native release failure')
                }
                return undefined as T
            }
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: replacement,
        })

        await facade.recover()
        await expect(facade.targetStatus()).rejects.toBeInstanceOf(NativeFileJobActivationCommittedError)
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })

        expect(refreshCommittedWorkingSet).toHaveBeenCalledOnce()
        expect(replacement.afterRefresh).toHaveBeenCalledOnce()
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_android_release')).toHaveLength(2)
        expect(release).toHaveBeenCalledOnce()
    })

    it('keeps a failed owned target until explicit cancellation resolves it', async () => {
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'failed',
                    completedBytes: 1,
                    totalBytes: 42,
                    error: 'invalid downloaded package',
                } as T
            }
            if (command === 'peer_clone_android_request_cancel') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const nativeBridge = bridge()
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: nativeBridge,
            runtime: runtime().replacement,
        })

        await facade.recover()

        expect(() => facade.join(pairingUri)).toThrow('already owns a clone job')
        await facade.cancel()
        expect(nativeBridge.cancel).toHaveBeenCalledWith('11111111-1111-4111-8111-111111111111')
    })

    it('does not replace or resume a persistently owned target job', async () => {
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_android_current') {
                return {
                    jobId: '11111111-1111-4111-8111-111111111111',
                    endpoint: 'http://192.168.1.4:43123/',
                    sessionId: '123e4567-e89b-42d3-a456-426614174000',
                    manifestId: 'a'.repeat(64),
                    phase: 'ready',
                    completedBytes: 1,
                } as T
            }
            if (command === 'peer_clone_android_request_cancel') return undefined as T
            throw new Error(`unexpected command ${command}`)
        })
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: bridge(),
            runtime: runtime().replacement,
        })
        await facade.recover()

        expect(() => facade.join(pairingUri)).toThrow('already owns a clone job')
        await facade.cancel()
        await expect(facade.resume()).rejects.toThrow('not paused')
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

    it('reports a disabled platform bridge and does not spend the one-time claim', async () => {
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
            throw new Error(`unexpected command ${command}`)
        })
        const nativeBridge = bridge()
        nativeBridge.transferMode = vi.fn(() => 'disabled' as const)
        const facade = createAndroidPeerCloneFacade({
            invoke: invoke as unknown as AndroidPeerCloneInvoke,
            bridge: nativeBridge,
            runtime: runtime().replacement,
        })

        expect(await facade.capabilities()).toMatchObject({ productionEnabled: false })
        facade.join(pairingUri)
        facade.confirmDestructiveReplace()
        await expect(facade.download()).rejects.toThrow('Android peer clone is not enabled by native production gates')
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_android_claim')).toBe(false)
    })
})
