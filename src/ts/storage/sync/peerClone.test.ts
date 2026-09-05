import { describe, expect, it, vi } from 'vitest'

import {
    createPeerCloneFacade,
    initialPeerCloneState,
    reducePeerCloneState,
} from './peerClone'
import type { PeerCloneInvoke, PeerCloneReplacementRuntime } from './peerClone'

const claim = 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'

const claimedTarget = {
    endpoint: 'http://192.168.1.4:43123/',
    sessionId: '123e4567-e89b-12d3-a456-426614174000',
    manifestId: 'a'.repeat(64),
}
const otherClaimedTarget = {
    ...claimedTarget,
    sessionId: '223e4567-e89b-42d3-a456-426614174000',
}

describe('PeerClone facade', () => {
    it('reuses an already claimed target without claiming again', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => (command === 'peer_clone_capabilities'
            ? {
                desktop: true, atomicActivationReady: true, losslessBackupReady: true,
                httpTransportReady: true, largeFixturePassed: true, productionEnabled: true,
            }
            : undefined) as T)
        const facade = createPeerCloneFacade({
            platform: 'desktop', invoke: invoke as unknown as PeerCloneInvoke, runtime: replacementRuntime(),
        })
        facade.joinClaimed({ endpoint: 'http://192.168.1.4:43123/', sessionId: 'session', manifestId: 'a'.repeat(64) })
        facade.confirmDestructiveReplace()
        await facade.download()

        expect(invoke.mock.calls.map(([command]) => command)).not.toContain('peer_clone_claim_client')
        expect(invoke.mock.calls.map(([command]) => command)).toContain('peer_clone_download')
    })

    it('sends the download request without the link claim once the source registration owns the target', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string, ..._args: unknown[]): Promise<T> => (command === 'peer_clone_capabilities'
            ? {
                desktop: true,
                atomicActivationReady: true,
                losslessBackupReady: true,
                httpTransportReady: true,
                largeFixturePassed: true,
                productionEnabled: true,
            }
            : undefined) as T)
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        facade.joinClaimed(claimedTarget)
        await expect(facade.download()).rejects.toThrow('confirmation')
        facade.confirmDestructiveReplace()
        await facade.download()

        expect(invoke).toHaveBeenCalledWith('peer_clone_download', claimedTarget)
        expect(invoke.mock.calls.map(([command]) => command)).not.toContain('peer_clone_claim_client')
        expect(JSON.stringify(invoke.mock.calls)).not.toContain(claim)
    })

    it('keeps target operations closed until native production gates pass', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(): Promise<T> => ({
            desktop: true,
            atomicActivationReady: false,
            losslessBackupReady: false,
            httpTransportReady: false,
            largeFixturePassed: false,
            productionEnabled: false,
        } as T))
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        await expect(facade.download()).rejects.toThrow('not enabled')
        facade.confirmDestructiveReplace()
        await expect(facade.resume()).rejects.toThrow('not enabled')

        expect(invoke.mock.calls).toEqual([
            ['peer_clone_capabilities'],
            ['peer_clone_capabilities'],
        ])
    })

    it.each([
        { name: 'lossless backup', losslessBackupReady: false, httpTransportReady: true },
        { name: 'HTTP transport', losslessBackupReady: true, httpTransportReady: false },
    ])('does not trust productionEnabled when the $name gate is closed', async ({ name: _name, ...gate }) => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(): Promise<T> => ({
            desktop: true,
            atomicActivationReady: true,
            largeFixturePassed: true,
            productionEnabled: true,
            ...gate,
        } as T))
        const facade = createPeerCloneFacade({ platform: 'desktop', invoke: invoke as unknown as PeerCloneInvoke })

        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        await expect(facade.download()).rejects.toThrow('not enabled')
        expect(invoke.mock.calls).toEqual([
            ['peer_clone_capabilities'],
        ])
    })

    it('treats the large fixture result as release evidence instead of a runtime gate', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => (command === 'peer_clone_capabilities'
            ? {
                desktop: true,
                atomicActivationReady: true,
                losslessBackupReady: true,
                httpTransportReady: true,
                largeFixturePassed: false,
                productionEnabled: true,
            }
            : undefined) as T)
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })

        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        await expect(facade.download()).resolves.toBeUndefined()
    })

    it('finalizes only after native download reaches the activation barrier', async () => {
        const events: string[] = []
        let finalized = false
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_download') {
                events.push('download')
                return undefined as T
            }
            if (command === 'peer_clone_target_status') {
                return {
                    phase: finalized ? 'completed' : 'awaitingActivation',
                    completedBytes: 10,
                    totalBytes: 10,
                } as T
            }
            if (command === 'peer_clone_finalize') {
                events.push('finalize')
                finalized = true
                return { revision: 42, backupPath: 'C:\\sync\\pre-clone.lossless' } as T
            }
            if (command === 'peer_clone_release_target') {
                events.push('native-release')
                return undefined as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken(reason: string) {
                    events.push(`capture:${reason}`)
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence(token: { revision: number; mutationGeneration: number }) {
                    events.push(`acquire:${token.revision}:${token.mutationGeneration}`)
                    return {
                        async refreshCommittedWorkingSet(revision: number) {
                            events.push(`refresh:${revision}`)
                        },
                        release() {
                            events.push('release')
                        },
                    }
                },
            },
        })

        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        await facade.download()
        expect(events).toEqual(['download'])

        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(events).toEqual([
            'download',
            'capture:peer-clone-target-finalize',
            'acquire:41:7',
            'finalize',
            'refresh:42',
            'native-release',
            'release',
        ])
        expect(facade.getState().target).toMatchObject({
            phase: 'completed',
            completedBytes: 10,
            totalBytes: 10,
            backupPaths: ['C:\\sync\\pre-clone.lossless'],
        })
    })

    it('releases the replacement fence when native finalize fails', async () => {
        const events: string[] = []
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') {
                events.push('finalize')
                throw new Error('revision conflict')
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    events.push('capture')
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    events.push('acquire')
                    return {
                        refreshCommittedWorkingSet: vi.fn(),
                        release() {
                            events.push('release')
                        },
                    }
                },
            },
        })

        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow('revision conflict')
        expect(events).toEqual(['capture', 'acquire', 'finalize', 'release'])
        expect(facade.getState().target.phase).toBe('failed')
    })

    it.each(['capture', 'acquire'] as const)('clears a failed %s handshake so finalization can retry', async (failure) => {
        let attempt = 0
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') return { revision: 42 } as T
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    if (failure === 'capture' && attempt++ === 0) throw new Error('capture failed')
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    if (failure === 'acquire' && attempt++ === 0) throw new Error('acquire failed')
                    return {
                        refreshCommittedWorkingSet: vi.fn(),
                        release: vi.fn(),
                    }
                },
            },
        })
        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow(`${failure} failed`)
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_finalize')).toHaveLength(1)
    })

    it('retains the committed revision and fence until renderer refresh succeeds', async () => {
        const events: string[] = []
        let finalized = false
        let refreshAttempt = 0
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return {
                    phase: finalized ? 'completed' : 'awaitingActivation',
                    completedBytes: 10,
                    totalBytes: 10,
                } as T
            }
            if (command === 'peer_clone_finalize') {
                events.push('finalize')
                finalized = true
                return { revision: 42, backupPath: 'C:\\sync\\refresh-retry.lossless' } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    events.push('capture')
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    events.push('acquire')
                    return {
                        async refreshCommittedWorkingSet(revision: number) {
                            events.push(`refresh:${revision}`)
                            if (refreshAttempt++ === 0) throw new Error('refresh failed')
                        },
                        release() {
                            events.push('release')
                        },
                    }
                },
            },
        })
        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow('refresh failed')
        expect(facade.getState().target.phase).not.toBe('failed')
        expect(facade.getState().target.backupPaths).toEqual(['C:\\sync\\refresh-retry.lossless'])
        expect(events).toEqual(['capture', 'acquire', 'finalize', 'refresh:42'])

        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(events).toEqual(['capture', 'acquire', 'finalize', 'refresh:42', 'refresh:42', 'release'])
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_release_target')).toHaveLength(1)
        expect(facade.getState().target.backupPaths).toEqual(['C:\\sync\\refresh-retry.lossless'])
    })

    it('retains the completed native target until release succeeds after one renderer refresh', async () => {
        let finalized = false
        let releaseAttempt = 0
        const refresh = vi.fn(async (_revision: number) => undefined)
        const fenceRelease = vi.fn()
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return {
                    phase: finalized ? 'completed' : 'awaitingActivation',
                    completedBytes: 10,
                    totalBytes: 10,
                } as T
            }
            if (command === 'peer_clone_finalize') {
                finalized = true
                return { revision: 42, backupPath: 'C:\\sync\\release-retry.lossless' } as T
            }
            if (command === 'peer_clone_release_target' && releaseAttempt++ === 0) {
                throw new Error('native release failed')
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(refresh, fenceRelease),
        })
        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).rejects.toThrow('native release failed')
        expect(facade.getState().target.phase).not.toBe('completed')
        expect(facade.getState().target.backupPaths).toEqual(['C:\\sync\\release-retry.lossless'])
        expect(fenceRelease).not.toHaveBeenCalled()
        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })

        expect(refresh).toHaveBeenCalledTimes(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_release_target')).toHaveLength(2)
        expect(fenceRelease).toHaveBeenCalledTimes(1)
        expect(facade.getState().target.backupPaths).toEqual(['C:\\sync\\release-retry.lossless'])
    })

    it('clears a previous clone backup receipt when a new target joins', () => {
        const completed = reducePeerCloneState(initialPeerCloneState, {
            type: 'target-completed',
            backupPaths: ['C:\\sync\\old.lossless'],
        })

        const joined = reducePeerCloneState(completed, {
            type: 'target-joined',
            pairing: { ...claimedTarget, claim },
        })

        expect(joined.target.backupPaths).toBeUndefined()
    })

    it('captures target identity before the asynchronous finalize handshake', async () => {
        let releaseCapture: (() => void) | undefined
        const captureBlocked = new Promise<void>((resolve) => {
            releaseCapture = resolve
        })
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') return { revision: 42 } as T
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                async capturePersistentMutationToken() {
                    await captureBlocked
                    return { revision: 41, mutationGeneration: 7 }
                },
                async acquireDestructiveReplacementFence() {
                    return {
                        refreshCommittedWorkingSet: vi.fn(),
                        release: vi.fn(),
                    }
                },
            },
        })
        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        const finalizing = facade.targetStatus()
        await Promise.resolve()
        expect(() => facade.joinClaimed(otherClaimedTarget)).toThrow('finalization is still active')
        releaseCapture?.()

        await finalizing
        expect(invoke).toHaveBeenCalledWith('peer_clone_finalize', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        })
    })

    it('does not finalize while native transfer is still downloading, and resume does not reclaim', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return productionCapabilities() as T
            if (command === 'peer_clone_target_status') {
                return { phase: 'downloading', completedBytes: 8, totalBytes: 10 } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: {
                flushPendingData: vi.fn(),
                capturePersistentMutationToken: vi.fn(),
                acquireDestructiveReplacementFence: vi.fn(),
            },
        })

        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        await facade.resume()
        await facade.targetStatus()

        expect(invoke).toHaveBeenCalledWith('peer_clone_resume', {
            endpoint: 'http://192.168.1.4:43123/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        })
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_claim_client')).toBe(false)
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_finalize')).toBe(false)
    })

    it.each(['capabilities', 'download'] as const)(
        'keeps the same claimed target resumable when the initial %s step fails',
        async (failure) => {
            let failed = false
            const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
                const failingCommand = failure === 'capabilities'
                    ? 'peer_clone_capabilities'
                    : 'peer_clone_download'
                if (command === failingCommand && !failed) {
                    failed = true
                    throw new Error(`${failure} failed`)
                }
                if (command === 'peer_clone_capabilities') return productionCapabilities() as T
                return undefined as T
            })
            const facade = createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: replacementRuntime(),
            })
            facade.joinClaimed(claimedTarget)
            facade.confirmDestructiveReplace()

            await expect(facade.download()).rejects.toThrow(`${failure} failed`)

            expect(facade.getState().target.phase).toBe('failed')
            await facade.resume()

            expect(invoke.mock.calls.map(([command]) => command)).not.toContain('peer_clone_claim_client')
            expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_download')).toHaveLength(
                failure === 'download' ? 1 : 0,
            )
            expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_resume')).toHaveLength(1)
            expect(facade.getState().target.phase).toBe('downloading')
        },
    )

    it('allows a different target only after committed refresh and native release complete', async () => {
        const events: string[] = []
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') return { revision: 42 } as T
            if (command === 'peer_clone_release_target') events.push(`native-release:${String(args?.sessionId)}`)
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(
                async () => { events.push('refresh') },
                () => { events.push('fence-release') },
            ),
        })
        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()

        await facade.targetStatus()
        expect(events).toEqual([
            'refresh',
            'native-release:123e4567-e89b-12d3-a456-426614174000',
            'fence-release',
        ])
        expect(facade.joinClaimed(otherClaimedTarget).target).toMatchObject({
            phase: 'joined',
            pairing: { sessionId: '223e4567-e89b-42d3-a456-426614174000' },
        })
    })

    it('discards a stale target status response after a different pairing joins', async () => {
        let resolveStatus: ((value: unknown) => void) | undefined
        const status = new Promise((resolve) => {
            resolveStatus = resolve
        })
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') return await status as T
            if (command === 'peer_clone_finalize') throw new Error('stale response finalized')
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })
        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        const polling = facade.targetStatus()
        facade.joinClaimed(otherClaimedTarget)
        resolveStatus?.({ phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 })
        await polling

        expect(facade.getState().target).toMatchObject({
            phase: 'joined',
            pairing: { sessionId: '223e4567-e89b-42d3-a456-426614174000' },
        })
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_finalize')).toBe(false)
    })

    it('retains a bounded native post-commit warning after refreshing the committed revision', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_target_status') {
                return { phase: 'awaitingActivation', completedBytes: 10, totalBytes: 10 } as T
            }
            if (command === 'peer_clone_finalize') {
                return { revision: 42, warning: 'activation ledger cleanup failed' } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: replacementRuntime(),
        })
        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()

        await expect(facade.targetStatus()).resolves.toMatchObject({ phase: 'completed' })
        expect(facade.getWarning()).toBe('activation ledger cleanup failed')
    })

    it('reports web and Android as explicitly unsupported without invoking native commands', async () => {
        const invoke = vi.fn()
        await expect(createPeerCloneFacade({ platform: 'web', invoke }).capabilities())
            .rejects.toThrow('Peer clone is unsupported on web')
        await expect(createPeerCloneFacade({ platform: 'android', invoke }).capabilities())
            .rejects.toThrow('Peer clone is unsupported on android')
        expect(invoke).not.toHaveBeenCalled()
    })

    it('models resumable target progress as pure transitions', () => {
        let state = initialPeerCloneState
        state = reducePeerCloneState(state, { type: 'target-joined', pairing: { ...claimedTarget, claim } })
        state = reducePeerCloneState(state, { type: 'target-confirmed' })
        state = reducePeerCloneState(state, { type: 'target-progress', completedBytes: 8, totalBytes: 10 })
        state = reducePeerCloneState(state, { type: 'target-cancelled' })
        state = reducePeerCloneState(state, { type: 'target-resumed' })

        expect(state).toMatchObject({
            target: { phase: 'downloading', destructiveConfirmed: true, completedBytes: 8, totalBytes: 10 },
        })
    })

    it('polls target progress for the claimed target', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => ({
            phase: command === 'peer_clone_target_status' ? 'downloading' : undefined,
            completedBytes: 8,
            totalBytes: 10,
        } as T))
        const facade = createPeerCloneFacade({ platform: 'desktop', invoke: invoke as unknown as PeerCloneInvoke })

        facade.joinClaimed(claimedTarget)
        facade.confirmDestructiveReplace()
        await facade.targetStatus()

        expect(facade.getState().target).toMatchObject({
            phase: 'downloading',
            completedBytes: 8,
            totalBytes: 10,
        })
        expect(invoke).toHaveBeenCalledWith('peer_clone_target_status', claimedTarget)
    })
})

function productionCapabilities() {
    return {
        desktop: true,
        atomicActivationReady: true,
        losslessBackupReady: true,
        httpTransportReady: true,
        largeFixturePassed: false,
        productionEnabled: true,
    }
}

function replacementRuntime(
    refreshCommittedWorkingSet: (revision: number) => Promise<void> = vi.fn(async (_revision: number) => undefined),
    release: () => void = vi.fn(),
): PeerCloneReplacementRuntime {
    return {
        flushPendingData: vi.fn(async () => undefined),
        capturePersistentMutationToken: vi.fn(async () => ({ revision: 1, mutationGeneration: 0 })),
        acquireDestructiveReplacementFence: vi.fn(async () => ({
            refreshCommittedWorkingSet,
            release,
        })),
    }
}
