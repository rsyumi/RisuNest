import { describe, expect, it, vi } from 'vitest'

import {
    createPeerBidirectionalFacade,
    parsePeerBidirectionalUri,
    PeerBidirectionalRefreshError,
    type PeerBidirectionalInvoke,
    type PeerBidirectionalMutationRuntime,
} from './peerBidirectional'

const pairingUri = 'risuailocal://peer-sync/v1?endpoint=http%3A%2F%2F192.168.1.20%3A32146'
    + '&session=123e4567-e89b-42d3-a456-426614174000'
    + `&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`

function runtime(log: string[]): PeerBidirectionalMutationRuntime {
    return {
        async flushPendingData(reason) {
            log.push(`flush:${reason}`)
        },
        async capturePersistentMutationToken(reason) {
            log.push(`capture:${reason}`)
            return { revision: 7, mutationGeneration: 11 }
        },
        async acquirePersistentMutationFence(token) {
            log.push(`acquire:${token.revision}:${token.mutationGeneration}`)
            return {
                async refreshCommittedWorkingSet(revision) {
                    log.push(`refresh:${revision}`)
                },
                release() {
                    log.push('release')
                },
            }
        },
    }
}

describe('peer bidirectional facade', () => {
    it('parses only the dedicated strict desktop pairing form', () => {
        expect(parsePeerBidirectionalUri(pairingUri)).toEqual({
            endpoint: 'http://192.168.1.20:32146',
            sessionId: '123e4567-e89b-42d3-a456-426614174000',
            manifestId: 'a'.repeat(64),
            claim: 'b'.repeat(64),
        })
        for (const invalid of [
            pairingUri.replace('peer-sync', 'peer-delta'),
            pairingUri.replace('/v1', '/v2'),
            pairingUri.replace('192.168.1.20', 'example.com'),
            pairingUri.replace(`#claim=${'b'.repeat(64)}`, ''),
            `${pairingUri}&extra=true`,
        ]) {
            expect(() => parsePeerBidirectionalUri(invalid)).toThrow('Invalid peer sync pairing URI')
        }
    })

    it.each(['web', 'android'] as const)('does not create a fallback authority on %s', async (platform) => {
        const facade = createPeerBidirectionalFacade({ platform })
        await expect(facade.capabilities()).rejects.toThrow(`Peer sync is unsupported on ${platform}`)
        await expect(facade.sync(pairingUri)).rejects.toThrow(`Peer sync is unsupported on ${platform}`)
    })

    it('flushes and fences the source revision from prepare until stop completes', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${String(args?.expectedRevision ?? '')}`)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_start') {
                return { phase: 'running', sessionId: 'session-source', devices: [] }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await facade.prepare()
        await facade.start('session-source')
        expect(events).toEqual([
            'flush:peer-bidirectional-source-prepare',
            'capture:peer-bidirectional-source-prepare',
            'acquire:7:11',
            'invoke:peer_bidirectional_prepare:7',
            'invoke:peer_bidirectional_start:',
        ])

        await facade.stop('session-source')
        expect(events.slice(-2)).toEqual([
            'invoke:peer_bidirectional_stop:',
            'release',
        ])
    })

    it('rejects a target mutation while source preparation is in flight', async () => {
        const events: string[] = []
        let finishPrepare!: (value: unknown) => void
        const invoke = vi.fn((command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return new Promise((resolve) => {
                    finishPrepare = resolve
                })
            }
            return Promise.resolve(undefined)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        const preparing = facade.prepare()
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith(
            'peer_bidirectional_prepare',
            { expectedRevision: 7 },
        ))
        await expect(facade.sync(pairingUri)).rejects.toThrow('peer sync source is active')
        finishPrepare({ phase: 'prepared', sessionId: 'session-source', devices: [] })
        await preparing
    })

    it('rejects source preparation while a target mutation is in flight', async () => {
        const events: string[] = []
        let finishSync!: (value: unknown) => void
        const invoke = vi.fn((command: string) => {
            if (command === 'peer_bidirectional_sync') {
                return new Promise((resolve) => {
                    finishSync = resolve
                })
            }
            return Promise.resolve(undefined)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        const syncing = facade.sync(pairingUri)
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith(
            'peer_bidirectional_sync',
            expect.objectContaining({ expectedRevision: 7 }),
        ))
        await expect(facade.prepare()).rejects.toThrow('source or target is already active')
        finishSync({
            kind: 'noChanges',
            operationId: 'operation-target-active',
            revision: 7,
            remoteRevision: 7,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        })
        await syncing
    })

    it('does not start a source host without its prepared mutation fence', async () => {
        const invoke = vi.fn(async () => ({
            phase: 'running' as const,
            sessionId: 'session-unfenced',
            devices: [],
        }))
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime([]),
        })

        await expect(facade.start('session-unfenced')).rejects.toThrow('not prepared')
        expect(invoke).not.toHaveBeenCalled()
    })

    it('refreshes a source-side committed revision once and retains its fence until stop', async () => {
        const events: string[] = []
        const completed = {
            phase: 'completed' as const,
            result: {
                kind: 'updated' as const,
                operationId: 'operation-source-complete',
                revision: 8,
                remoteRevision: 9,
                transferredObjects: 2,
                transferredBytes: 12,
                backups: [{ packageId: 'backup-source', side: 'local' as const, path: 'source.risulossless' }],
            },
        }
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'running', sessionId: 'session-source', devices: [] },
                    operation: completed,
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await facade.prepare()
        await facade.status()
        await facade.status()
        expect(events.filter((event) => event === 'refresh:8')).toHaveLength(1)
        expect(events).not.toContain('release')

        await facade.stop('session-source')
        expect(events.at(-1)).toBe('release')
    })

    it('retries only a failed source refresh before polling native status again', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'running', sessionId: 'session-source', devices: [] },
                    operation: {
                        phase: 'completed',
                        result: {
                            kind: 'noChanges',
                            operationId: 'operation-source-refresh',
                            revision: 7,
                            remoteRevision: 7,
                            transferredObjects: 0,
                            transferredBytes: 0,
                            backups: [],
                        },
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('source renderer refresh failed')
                            }
                        },
                    }
                },
            },
        })

        await facade.prepare()
        await expect(facade.status()).rejects.toThrow('source renderer refresh failed')
        await expect(facade.status()).resolves.toMatchObject({ operation: { phase: 'completed' } })
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_status')).toHaveLength(1)
        expect(events.filter((event) => event === 'refresh:7')).toHaveLength(2)
        expect(events).not.toContain('release')
    })

    it('closes the source host before finishing a pending refresh and releasing its fence', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'running', sessionId: 'session-source', devices: [] },
                    operation: {
                        phase: 'completed',
                        result: {
                            kind: 'updated',
                            operationId: 'operation-stop-refresh',
                            revision: 8,
                            remoteRevision: 9,
                            transferredObjects: 1,
                            transferredBytes: 12,
                            backups: [],
                        },
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('source refresh failed')
                            }
                        },
                    }
                },
            },
        })

        await facade.prepare()
        await expect(facade.status()).rejects.toThrow('source refresh failed')
        await facade.stop('session-source')
        expect(events.slice(-3)).toEqual([
            'invoke:peer_bidirectional_stop',
            'refresh:8',
            'release',
        ])
    })

    it('keeps production disabled unless every native readiness field is true', async () => {
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: (async () => ({
                desktop: true,
                sourceReady: true,
                atomicActivationReady: true,
                authenticatedTransportReady: false,
                losslessBackupReady: true,
                durableStateReady: true,
                productionEnabled: true,
            })) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.capabilities()).resolves.toMatchObject({ productionEnabled: false })
    })

    it('holds the mutation fence through native completion and renderer refresh', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${String(args?.expectedRevision ?? '')}`)
            return {
                kind: 'updated',
                operationId: 'operation-1',
                revision: 8,
                remoteRevision: 4,
                transferredObjects: 2,
                transferredBytes: 19,
                backups: [],
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await expect(facade.sync(pairingUri)).resolves.toMatchObject({ kind: 'updated', revision: 8 })
        expect(events).toEqual([
            'flush:peer-bidirectional-sync',
            'capture:peer-bidirectional-sync',
            'acquire:7:11',
            'invoke:peer_bidirectional_sync:7',
            'refresh:8',
            'release',
        ])
    })

    it.each([
        {
            kind: 'resumeRequired' as const,
            operationId: 'operation-local-committed',
            phase: 'localCommitted' as const,
            committedRevision: 8,
        },
        {
            kind: 'sourceUnavailable' as const,
            operationId: 'operation-source-unavailable',
            committedRevision: 8,
        },
    ])('refreshes the committed working set before returning $kind', async (result) => {
        const events: string[] = []
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: (async () => result) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resume(result.operationId)).resolves.toBe(result)
        expect(events.slice(-2)).toEqual(['refresh:8', 'release'])
    })

    it('refreshes a durable local commit before surfacing a lost native response', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_sync') throw new Error('native response lost')
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-response-loss',
                        committedRevision: 8,
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.sync(pairingUri)).rejects.toThrow('native response lost')
        expect(events.slice(-3)).toEqual([
            'invoke:peer_bidirectional_status',
            'refresh:8',
            'release',
        ])
    })

    it('returns explicit same-record conflicts without refreshing the renderer', async () => {
        const events: string[] = []
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: (async () => ({
                kind: 'conflict',
                operationId: 'operation-2',
                conflicts: [
                    { key: 'r1:root', type: 'sameRecord' },
                    { key: 'r1:preset:WyIwIl0', type: 'deleteVsEdit' },
                ],
                localManifestHash: 'c'.repeat(64),
                remoteManifestHash: 'd'.repeat(64),
            })) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.sync(pairingUri)).resolves.toMatchObject({
            kind: 'conflict',
            operationId: 'operation-2',
        })
        expect(events).not.toContain('refresh:7')
        expect(events.filter((event) => event.startsWith('refresh:'))).toHaveLength(0)
        expect(events.at(-1)).toBe('release')
    })

    it('does not refresh the renderer for a stale generation result', async () => {
        const events: string[] = []
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: (async () => ({
                kind: 'stale',
                operationId: 'operation-stale',
                reason: 'remoteGeneration',
            })) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.sync(pairingUri)).resolves.toMatchObject({ kind: 'stale' })
        expect(events.filter((event) => event.startsWith('refresh:'))).toHaveLength(0)
        expect(events.at(-1)).toBe('release')
    })

    it('retries only renderer refresh after native completion was committed', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async () => ({
            kind: 'noChanges' as const,
            operationId: 'operation-3',
            revision: 7,
            remoteRevision: 5,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }))
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('renderer refresh failed')
                            }
                        },
                    }
                },
            },
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.sync(pairingUri)).rejects.toThrow('renderer refresh failed')
        await expect(facade.sync(pairingUri)).resolves.toMatchObject({ kind: 'noChanges' })
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
    })

    it('resolves a retained conflict by operation id and explicit winner', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${String(args?.operationId)}:${String(args?.winner)}`)
            return {
                kind: 'updated',
                operationId: 'operation-4',
                revision: 9,
                remoteRevision: 6,
                transferredObjects: 1,
                transferredBytes: 8,
                backups: [{ packageId: 'backup-1', side: 'remote', path: 'conflict.risulossless' }],
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await facade.resolve('operation-4', 'local')
        expect(events).toContain('invoke:peer_bidirectional_resolve:operation-4:local')
        expect(events).toContain('refresh:9')
    })

    it('retains the exact resolve result and fence when renderer refresh fails', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async () => ({
            kind: 'updated' as const,
            operationId: 'operation-refresh',
            revision: 9,
            remoteRevision: 6,
            transferredObjects: 1,
            transferredBytes: 8,
            backups: [],
        }))
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('renderer refresh failed')
                            }
                        },
                    }
                },
            },
        })

        await expect(facade.resolve('operation-refresh', 'local')).rejects.toBeInstanceOf(PeerBidirectionalRefreshError)
        await expect(facade.resolve('operation-refresh', 'remote')).rejects.toThrow('different peer sync retry')
        await expect(facade.resolve('operation-refresh', 'local')).resolves.toMatchObject({ kind: 'updated' })
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
    })
})
