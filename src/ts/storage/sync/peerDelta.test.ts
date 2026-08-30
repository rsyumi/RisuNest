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
        expect(parsePeerDeltaUri(pairing.replace(
            'http%3A%2F%2F192.168.1.20%3A32145%2F',
            'https%3A%2F%2Fsync.example.com',
        )).endpoint).toBe('https://sync.example.com/')
        expect(() => parsePeerDeltaUri(`${pairing}&extra=1`)).toThrow()
    })

    test.each([
        'https%3A%2F%2F127.0.0.1',
        'https%3A%2F%2Flocalhost',
        'https%3A%2F%2Fsync.example.com%3A443',
        'https%3A%2F%2Fsync.example.com%2Fpath',
        'https%3A%2F%2Fsync.example.com%3Fquery%3D1',
        'https%3A%2F%2Fuser%40sync.example.com',
    ])('rejects malformed or non-canonical public HTTPS endpoint %s', (endpoint) => {
        expect(() => parsePeerDeltaUri(pairing.replace(
            'http%3A%2F%2F192.168.1.20%3A32145%2F',
            endpoint,
        ))).toThrow('Invalid peer delta pairing URI')
    })

    test('starts and stops Quick and Named tunnel transports through P4-owned commands', async () => {
        const invokeMock = vi.fn(async <T>(command: string): Promise<T> => ({
            phase: 'running',
            sessionId: 'session',
            manifestId: 'a'.repeat(64),
            pairingUri: 'risuailocal://peer-delta/v1',
            tunnel: { kind: command.includes('status') ? 'quick' : 'named', experimental: false, oneShot: false },
            devices: [],
        }) as T)
        const invoke = invokeMock as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'desktop', invoke })

        await facade.startQuickTunnel('session')
        await facade.startNamedTunnel('session', 'secret-token', 'https://sync.example.com')
        await facade.tunnelStatus()
        await facade.stopTunnel('session')

        expect(invokeMock.mock.calls).toEqual([
            ['peer_delta_tunnel_start', { sessionId: 'session', tunnel: { kind: 'quick' } }],
            ['peer_delta_tunnel_start', {
                sessionId: 'session',
                tunnel: { kind: 'named', token: 'secret-token', expectedPublicBaseUrl: 'https://sync.example.com' },
            }],
            ['peer_delta_tunnel_status'],
            ['peer_delta_tunnel_stop', { sessionId: 'session' }],
        ])
    })

    test.each([
        ['127.23.4.5', 'http://127.23.4.5:32145/'],
        ['%5B%3A%3A1%5D', 'http://[::1]:32145/'],
    ])('accepts the supported LAN loopback endpoint %s', (encodedHost, endpoint) => {
        const loopbackPairing = pairing.replace('192.168.1.20', encodedHost)
        expect(parsePeerDeltaUri(loopbackPairing).endpoint).toBe(endpoint)
    })

    test('does not expose native delta on web', async () => {
        const platform = 'web' as const
        const facade = createPeerDeltaFacade({ platform })
        await expect(facade.capabilities()).rejects.toThrow(`unsupported on ${platform}`)
        await expect(facade.pull(pairing)).rejects.toThrow(`unsupported on ${platform}`)
    })

    test('runs Android P4 target through a foreground identity with renderer ordering intact', async () => {
        const events: string[] = []
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
        const bridge = {
            startSource: vi.fn(() => { events.push('service-start'); return true }),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const runtime: PeerDeltaMutationRuntime = {
            async flushPendingData() { events.push('flush') },
            async capturePersistentMutationToken() { events.push('capture'); return { revision: 4, mutationGeneration: 2 } },
            async acquirePersistentMutationFence() {
                events.push('fence')
                return {
                    async refreshCommittedWorkingSet(revision) { events.push(`refresh:${revision}`) },
                    release() { events.push('release') },
                }
            },
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            events.push(command)
            if (command.endsWith('_reserve')) return foreground as T
            return { kind: 'updated', revision: 5, transferredObjects: 1, transferredBytes: 8 } as T
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await facade.pull(pairing)

        expect(events).toEqual([
            'flush', 'capture', 'fence', 'peer_delta_target_reserve', 'service-start',
            'peer_delta_pull', 'refresh:5', 'release', 'service-stop',
        ])
        expect('startQuickTunnel' in facade).toBe(false)
        expect('startNamedTunnel' in facade).toBe(false)
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
