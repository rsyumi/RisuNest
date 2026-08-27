import { describe, expect, it, vi } from 'vitest'

import {
    createPeerBidirectionalFacade,
    parsePeerBidirectionalUri,
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
})
