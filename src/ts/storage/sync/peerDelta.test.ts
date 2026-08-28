import { describe, expect, test, vi } from 'vitest'

import {
    createPeerDeltaFacade,
    parsePeerDeltaUri,
    type PeerDeltaInvoke,
    type PeerDeltaMutationRuntime,
} from './peerDelta'

const pairing = 'risuailocal://peer-delta/v1?endpoint=http%3A%2F%2F192.168.1.20%3A32145%2F&session=00000000-0000-4000-8000-000000000001&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'

describe('peer logical delta product facade', () => {
    test('parses only the dedicated strict delta pairing URI', () => {
        expect(parsePeerDeltaUri(pairing)).toEqual({
            endpoint: 'http://192.168.1.20:32145/',
            sessionId: '00000000-0000-4000-8000-000000000001',
            manifestId: 'a'.repeat(64),
            claim: 'b'.repeat(64),
        })
        expect(() => parsePeerDeltaUri(pairing.replace('peer-delta', 'peer-clone'))).toThrow()
        expect(() => parsePeerDeltaUri(pairing.replace('192.168.1.20', '8.8.8.8'))).toThrow()
        expect(() => parsePeerDeltaUri(pairing.replace(
            'http%3A%2F%2F192.168.1.20%3A32145%2F',
            'https%3A%2F%2Fsync.example.com',
        ))).toThrow()
        expect(() => parsePeerDeltaUri(`${pairing}&extra=1`)).toThrow()
    })

    test.each(['web', 'android'] as const)('does not expose native delta on %s', async (platform) => {
        const facade = createPeerDeltaFacade({ platform })
        await expect(facade.capabilities()).rejects.toThrow(`unsupported on ${platform}`)
        await expect(facade.pull(pairing)).rejects.toThrow(`unsupported on ${platform}`)
    })

    test('flushes and fences the exact revision while native Rust applies and renderer refreshes', async () => {
        const events: string[] = []
        const runtime: PeerDeltaMutationRuntime = {
            async flushPendingData() {
                events.push('flush')
            },
            async capturePersistentMutationToken() {
                events.push('capture')
                return { revision: 14, mutationGeneration: 3 }
            },
            async acquirePersistentMutationFence(token) {
                events.push(`fence:${token.revision}:${token.mutationGeneration}`)
                return {
                    async refreshCommittedWorkingSet(revision) {
                        events.push(`refresh:${revision}`)
                    },
                    release() {
                        events.push('release')
                    },
                }
            },
        }
        const invoke = vi.fn(async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
            events.push(`invoke:${command}:${String(args?.expectedRevision)}`)
            return {
                kind: 'updated',
                revision: 15,
                transferredObjects: 2,
                transferredBytes: 4096,
            } as T
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'desktop', invoke, runtime })

        await expect(facade.pull(pairing)).resolves.toEqual({
            kind: 'updated',
            revision: 15,
            transferredObjects: 2,
            transferredBytes: 4096,
        })
        expect(events).toEqual([
            'flush',
            'capture',
            'fence:14:3',
            'invoke:peer_delta_pull:14',
            'refresh:15',
            'release',
        ])
        expect(invoke).toHaveBeenCalledWith('peer_delta_pull', {
            endpoint: 'http://192.168.1.20:32145/',
            sessionId: '00000000-0000-4000-8000-000000000001',
            manifestId: 'a'.repeat(64),
            claim: 'b'.repeat(64),
            expectedRevision: 14,
        })
    })

    test('does not refresh a divergent library and always releases the mutation fence', async () => {
        const refresh = vi.fn()
        const release = vi.fn()
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 7, mutationGeneration: 1 })),
            acquirePersistentMutationFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: refresh,
                release,
            })),
        }
        const invoke = vi.fn(async <T>(): Promise<T> => ({
            kind: 'fullCloneRequired',
            reason: 'noExactCommonBase',
        }) as T) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'desktop', invoke, runtime })

        await expect(facade.pull(pairing)).resolves.toEqual({
            kind: 'fullCloneRequired',
            reason: 'noExactCommonBase',
        })
        expect(refresh).not.toHaveBeenCalled()
        expect(release).toHaveBeenCalledOnce()
    })

    test('releases the mutation fence when the native operation fails', async () => {
        const release = vi.fn()
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 3, mutationGeneration: 2 })),
            acquirePersistentMutationFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(),
                release,
            })),
        }
        const facade = createPeerDeltaFacade({
            platform: 'desktop',
            invoke: vi.fn(async () => { throw new Error('stale revision') }),
            runtime,
        })

        await expect(facade.pull(pairing)).rejects.toThrow('stale revision')
        expect(release).toHaveBeenCalledOnce()
    })

    test('retains a committed revision fence and retries only renderer refresh', async () => {
        const release = vi.fn()
        const refresh = vi.fn()
            .mockRejectedValueOnce(new Error('renderer refresh failed'))
            .mockResolvedValueOnce(undefined)
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 8, mutationGeneration: 2 })),
            acquirePersistentMutationFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: refresh,
                release,
            })),
        }
        const result = {
            kind: 'updated',
            revision: 9,
            transferredObjects: 1,
            transferredBytes: 32,
        } as const
        const invoke = vi.fn(async <T>(): Promise<T> => result as T) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'desktop', invoke, runtime })

        await expect(facade.pull(pairing)).rejects.toThrow('renderer refresh failed')
        expect(release).not.toHaveBeenCalled()

        await expect(facade.pull(pairing)).resolves.toEqual(result)
        expect(invoke).toHaveBeenCalledOnce()
        expect(runtime.flushPendingData).toHaveBeenCalledOnce()
        expect(runtime.capturePersistentMutationToken).toHaveBeenCalledOnce()
        expect(runtime.acquirePersistentMutationFence).toHaveBeenCalledOnce()
        expect(refresh).toHaveBeenCalledTimes(2)
        expect(release).toHaveBeenCalledOnce()
    })
})
