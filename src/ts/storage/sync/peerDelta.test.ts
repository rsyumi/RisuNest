import { describe, expect, test, vi } from 'vitest'

import {
    createPeerDeltaFacade,
    parsePeerDeltaUri,
    type PeerDeltaInvoke,
    type PeerDeltaMutationRuntime,
} from './peerDelta'

const pairing = 'risuailocal://peer-delta/v1?endpoint=http%3A%2F%2F192.168.1.20%3A32145%2F&session=00000000-0000-4000-8000-000000000001&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'

describe('peer logical delta product facade', () => {
    test.each([
        ['false', false],
        ['throw', true],
        ['timeout', true],
    ] as const)(
        'abandons exact P4 source reservation after %s only with definitive failure or accepted Stop',
        async (failure, expectsStop) => {
            const foreground = { lane: 'p4-source', operationId: '44444444-4444-4444-8444-444444444444', generation: 9 } as const
            let owner: typeof foreground | undefined
            const bridge = {
                startSource: vi.fn(() => {
                    if (failure === 'throw') throw new Error('service throw')
                    return failure !== 'false'
                }),
                stopSource: vi.fn(() => true),
            }
            const invoke = vi.fn(async <T>(command: string): Promise<T> => {
                if (command === 'peer_delta_source_reserve') {
                    if (owner) throw new Error('owner retained')
                    owner = foreground
                    return foreground as T
                }
                if (command === 'peer_sync_foreground_source_abandon') {
                    owner = undefined
                    return true as T
                }
                if (command === 'peer_delta_start' && failure === 'timeout') throw new Error('attach timeout')
                return { phase: 'running', devices: [] } as T
            }) as PeerDeltaInvoke
            const facade = createPeerDeltaFacade({ platform: 'android', invoke, bridge })

            await expect(facade.start('session')).rejects.toThrow()
            if (expectsStop) {
                expect(bridge.stopSource).toHaveBeenCalledWith(
                    foreground.lane,
                    foreground.operationId,
                    foreground.generation,
                )
            } else {
                expect(bridge.stopSource).not.toHaveBeenCalled()
            }
            expect(invoke).toHaveBeenCalledWith('peer_sync_foreground_source_abandon', { foreground })
            expect(owner).toBeUndefined()
        },
    )

    test('keeps the P4 source start command vector exact', async () => {
        const foreground = { lane: 'p4-source', operationId: '45454545-4545-4545-8545-454545454545', generation: 10 } as const
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_sync_foreground_source_status') return null as T
            if (command === 'peer_delta_source_reserve') return foreground as T
            if (command === 'peer_delta_start') return { phase: 'running', devices: [] } as T
            throw new Error(`Unexpected command: ${command}`)
        })
        const facade = createPeerDeltaFacade({
            platform: 'android',
            invoke: invoke as PeerDeltaInvoke,
            bridge: { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) },
        })

        await facade.start('session')

        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'peer_sync_foreground_source_status',
            'peer_delta_source_reserve',
            'peer_delta_start',
        ])
    })

    test.each(['false', 'throw'] as const)(
        'retains uncertain P4 source ownership when exact Stop returns %s and recovers before reserve',
        async (stopFailure) => {
            const foreground = { lane: 'p4-source', operationId: '44444444-4444-4444-8444-444444444444', generation: 9 } as const
            let owner: typeof foreground | undefined
            let stopAttempt = 0
            let startThrows = true
            const bridge = {
                startSource: vi.fn(() => {
                    if (startThrows) throw new Error('uncertain START')
                    return true
                }),
                stopSource: vi.fn(() => {
                    stopAttempt += 1
                    if (stopAttempt === 1) {
                        if (stopFailure === 'throw') throw new Error('uncertain Stop')
                        return false
                    }
                    return true
                }),
            }
            const invoke = vi.fn(async <T>(command: string): Promise<T> => {
                if (command === 'peer_sync_foreground_source_status') return (owner ?? null) as T
                if (command === 'peer_delta_source_reserve') {
                    if (owner) throw new Error('owner retained')
                    owner = foreground
                    return foreground as T
                }
                if (command === 'peer_sync_foreground_source_abandon') {
                    owner = undefined
                    return true as T
                }
                return { phase: 'running', devices: [] } as T
            }) as PeerDeltaInvoke
            const facade = createPeerDeltaFacade({ platform: 'android', invoke, bridge })

            await expect(facade.start('session')).rejects.toThrow()
            expect(owner).toEqual(foreground)
            expect(invoke).not.toHaveBeenCalledWith('peer_sync_foreground_source_abandon', { foreground })

            startThrows = false
            await facade.start('session')

            expect(stopAttempt).toBe(2)
            expect(owner).toEqual(foreground)
        },
    )

    test('retains both the P4 primary start error and exact-abandon failure', async () => {
        const foreground = { lane: 'p4-source', operationId: '44444444-4444-4444-8444-444444444444', generation: 9 } as const
        const primary = new Error('attach timeout')
        const cleanup = new Error('abandon response lost')
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_sync_foreground_source_status') return null as T
            if (command === 'peer_delta_source_reserve') return foreground as T
            if (command === 'peer_sync_foreground_source_abandon') throw cleanup
            throw primary
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({
            platform: 'android',
            invoke,
            bridge: { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) },
        })

        const failure = await facade.start('session').catch((error: unknown) => error)

        expect(failure).toBeInstanceOf(AggregateError)
        expect((failure as AggregateError).errors).toEqual([primary, cleanup])
    })

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

    test('flushes pending renderer data before native seals the served source generation', async () => {
        const events: string[] = []
        const runtime: PeerDeltaMutationRuntime = {
            async flushPendingData(reason) { events.push(`flush:${reason}`) },
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 1, mutationGeneration: 0 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(async () => undefined),
                release: vi.fn(),
            })),
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            events.push(command)
            return { phase: 'prepared', sessionId: 'session', devices: [] } as T
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'desktop', invoke, runtime })

        await expect(facade.prepare()).resolves.toMatchObject({ phase: 'prepared' })

        expect(events).toEqual(['flush:peer-delta-source-prepare', 'peer_delta_prepare'])
    })

    test('refuses to prepare a source without the mutation runtime', async () => {
        const invoke = vi.fn(async <T>(): Promise<T> => ({ phase: 'prepared', devices: [] }) as T) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'desktop', invoke })

        await expect(facade.prepare()).rejects.toThrow('Peer delta mutation runtime is unavailable')
        expect(invoke).not.toHaveBeenCalled()
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

    test('fails closed when the Android foreground service rejects an exact source stop', async () => {
        const foreground = { lane: 'p4-source', operationId: '44444444-4444-4444-8444-444444444444', generation: 9 } as const
        const bridge = { startSource: vi.fn(() => true), stopSource: vi.fn(() => false) }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_stop') return foreground as T
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, bridge })

        await expect(facade.stop('session')).rejects.toThrow(
            'Android peer delta source foreground service could not stop',
        )
        expect(bridge.stopSource).toHaveBeenCalledWith(
            foreground.lane,
            foreground.operationId,
            foreground.generation,
        )
    })

    test('fails closed when an Android stop returns an identity without a foreground bridge', async () => {
        const foreground = { lane: 'p4-source', operationId: '44444444-4444-4444-8444-444444444444', generation: 9 } as const
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_stop') return foreground as T
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke })

        await expect(facade.stop('session')).rejects.toThrow(
            'Android peer delta foreground service is unavailable',
        )
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
            async acquireDestructiveReplacementFence() {
                events.push('fence')
                return {
                    async refreshCommittedWorkingSet(revision) { events.push(`refresh:${revision}`) },
                    release() { events.push('release') },
                }
            },
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            events.push(command)
            if (command === 'peer_delta_target_foreground_status') return null as T
            if (command.endsWith('_reserve')) return foreground as T
            return { kind: 'updated', revision: 5, transferredObjects: 1, transferredBytes: 8 } as T
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await facade.pull(pairing)

        expect(events).toEqual([
            'peer_delta_target_foreground_status', 'flush', 'capture', 'fence',
            'peer_delta_target_reserve', 'service-start', 'peer_delta_pull',
            'refresh:5', 'release', 'service-stop', 'peer_delta_target_foreground_release',
        ])
        await expect(facade.startQuickTunnel('session')).rejects.toThrow('Peer delta is unsupported on android')
        await expect(facade.startNamedTunnel('session', 'token', 'https://sync.example.com'))
            .rejects.toThrow('Peer delta is unsupported on android')
        await expect(facade.tunnelStatus()).rejects.toThrow('Peer delta is unsupported on android')
        await expect(facade.stopTunnel('session')).rejects.toThrow('Peer delta is unsupported on android')
    })

    test('runs a registered Android pull through the legacy mutation and foreground lifecycle', async () => {
        const events: string[] = []
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
        const bridge = {
            startSource: vi.fn(() => { events.push('service-start'); return true }),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const runtime: PeerDeltaMutationRuntime = {
            async flushPendingData(reason) { events.push(`flush:${reason}`) },
            async capturePersistentMutationToken(reason) {
                events.push(`capture:${reason}`)
                return { revision: 4, mutationGeneration: 2 }
            },
            async acquireDestructiveReplacementFence() {
                events.push('fence')
                return {
                    async refreshCommittedWorkingSet(revision) { events.push(`refresh:${revision}`) },
                    release() { events.push('release') },
                }
            },
        }
        const invoke = vi.fn(async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
            events.push(`${command}:${JSON.stringify(args ?? {})}`)
            if (command === 'peer_delta_target_foreground_status') return null as T
            if (command === 'peer_delta_target_reserve') return foreground as T
            if (command === 'peer_delta_target_foreground_release') return true as T
            return { kind: 'updated', revision: 5, transferredObjects: 1, transferredBytes: 8 } as T
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await facade.pullRegistered('source-device')

        expect(events).toEqual([
            'peer_delta_target_foreground_status:{}',
            'flush:peer-delta-pull',
            'capture:peer-delta-pull',
            'fence',
            'peer_delta_target_reserve:{}',
            'service-start',
            `peer_delta_pull_registered:${JSON.stringify({
                deviceId: 'source-device', expectedRevision: 4, foreground,
            })}`,
            'refresh:5',
            'release',
            'service-stop',
            `peer_delta_target_foreground_release:${JSON.stringify({ foreground })}`,
        ])
    })

    test('current renderer cleans exact Reserved target after foreground START throws', async () => {
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
        const events: string[] = []
        let owner: typeof foreground | undefined
        const primary = new Error('foreground START uncertain')
        const bridge = {
            startSource: vi.fn(() => { events.push('service-start'); throw primary }),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const runtime: PeerDeltaMutationRuntime = {
            async flushPendingData() { events.push('flush') },
            async capturePersistentMutationToken() { events.push('capture'); return { revision: 4, mutationGeneration: 2 } },
            async acquireDestructiveReplacementFence() {
                events.push('fence')
                return { refreshCommittedWorkingSet: vi.fn(), release: () => { events.push('release') } }
            },
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            events.push(command)
            if (command === 'peer_delta_target_foreground_status') {
                return (owner ? { foreground: owner, phase: 'reserved' } : null) as T
            }
            if (command === 'peer_delta_target_reserve') {
                owner = foreground
                return foreground as T
            }
            if (command === 'peer_delta_target_foreground_release') {
                owner = undefined
                return true as T
            }
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await expect(facade.pull(pairing)).rejects.toBe(primary)

        expect(owner).toBeUndefined()
        expect(events).toEqual([
            'peer_delta_target_foreground_status', 'flush', 'capture', 'fence',
            'peer_delta_target_reserve', 'service-start',
            'peer_delta_target_foreground_status', 'release',
            'service-stop', 'peer_delta_target_foreground_release',
        ])
    })

    test('current renderer releases Terminal failure without refreshing data authority', async () => {
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
        const events: string[] = []
        let phase: 'absent' | 'reserved' | 'terminal' = 'absent'
        const primary = new Error('native pull rejected')
        const bridge = {
            startSource: vi.fn(() => { phase = 'reserved'; return true }),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 4, mutationGeneration: 2 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(async () => { events.push('refresh') }),
                release: () => { events.push('release') },
            })),
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_target_foreground_status') {
                if (phase === 'absent') return null as T
                return { foreground, phase: 'terminal', error: 'cancelled before activation' } as T
            }
            if (command === 'peer_delta_target_reserve') return foreground as T
            if (command === 'peer_delta_pull') {
                phase = 'terminal'
                throw primary
            }
            if (command === 'peer_delta_target_foreground_release') {
                phase = 'absent'
                events.push('native-release')
                return true as T
            }
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await expect(facade.pull(pairing)).rejects.toBe(primary)

        expect(events).toEqual(['release', 'service-stop', 'native-release'])
        expect(phase).toBe('absent')
    })

    test('current renderer refreshes committed Terminal result under its existing fence before cleanup', async () => {
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
        const events: string[] = []
        let phase: 'absent' | 'terminal' = 'absent'
        const primary = new Error('native response lost after commit')
        const bridge = {
            startSource: vi.fn(() => true),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const runtime: PeerDeltaMutationRuntime = {
            async flushPendingData() { events.push('flush') },
            async capturePersistentMutationToken() { events.push('capture'); return { revision: 4, mutationGeneration: 2 } },
            async acquireDestructiveReplacementFence() {
                events.push('fence')
                return {
                    async refreshCommittedWorkingSet(revision) { events.push(`refresh:${revision}`) },
                    release() { events.push('release') },
                }
            },
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            events.push(command)
            if (command === 'peer_delta_target_foreground_status') {
                if (phase === 'absent') return null as T
                return {
                    foreground,
                    phase: 'terminal',
                    result: { kind: 'updated', revision: 5, transferredObjects: 1, transferredBytes: 8 },
                } as T
            }
            if (command === 'peer_delta_target_reserve') return foreground as T
            if (command === 'peer_delta_pull') {
                phase = 'terminal'
                throw primary
            }
            if (command === 'peer_delta_target_foreground_release') {
                phase = 'absent'
                return true as T
            }
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await expect(facade.pull(pairing)).rejects.toBe(primary)

        expect(events).toEqual([
            'peer_delta_target_foreground_status', 'flush', 'capture', 'fence',
            'peer_delta_target_reserve', 'peer_delta_pull',
            'peer_delta_target_foreground_status', 'refresh:5', 'release',
            'service-stop', 'peer_delta_target_foreground_release',
        ])
    })

    test('current renderer cancels Running after a lost pull response and waits for Terminal', async () => {
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
        const events: string[] = []
        let phase: 'absent' | 'running' | 'terminal' = 'absent'
        const primary = new Error('native pull response lost')
        const bridge = {
            startSource: vi.fn(() => true),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 4, mutationGeneration: 2 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(),
                release: () => { events.push('release') },
            })),
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_target_foreground_status') {
                events.push(`status:${phase}`)
                if (phase === 'absent') return null as T
                return { foreground, phase, ...(phase === 'terminal' ? { error: 'cancelled' } : {}) } as T
            }
            if (command === 'peer_delta_target_reserve') return foreground as T
            if (command === 'peer_delta_pull') {
                phase = 'running'
                throw primary
            }
            if (command === 'peer_delta_target_foreground_cancel') {
                events.push('cancel')
                phase = 'terminal'
                return true as T
            }
            if (command === 'peer_delta_target_foreground_release') {
                events.push('native-release')
                phase = 'absent'
                return true as T
            }
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await expect(facade.pull(pairing)).rejects.toBe(primary)

        expect(events).toEqual([
            'status:absent', 'status:running', 'cancel', 'status:terminal',
            'release', 'service-stop', 'native-release',
        ])
    })

    test('current renderer preserves primary pull failure when exact target cleanup fails', async () => {
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
        let phase: 'absent' | 'terminal' = 'absent'
        const primary = new Error('native pull rejected')
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_target_foreground_status') {
                return (phase === 'terminal'
                    ? { foreground, phase: 'terminal', error: 'precommit failure' }
                    : null) as T
            }
            if (command === 'peer_delta_target_reserve') return foreground as T
            if (command === 'peer_delta_pull') {
                phase = 'terminal'
                throw primary
            }
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 4, mutationGeneration: 2 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(),
                release: vi.fn(),
            })),
        }
        const facade = createPeerDeltaFacade({
            platform: 'android',
            invoke,
            runtime,
            bridge: { startSource: vi.fn(() => true), stopSource: vi.fn(() => false) },
        })

        const failure = await facade.pull(pairing).catch((error: unknown) => error)

        expect(failure).toBeInstanceOf(AggregateError)
        expect((failure as AggregateError).errors[0]).toBe(primary)
        expect((failure as AggregateError).errors[1]).toEqual(
            new Error('Android peer delta foreground service could not stop'),
        )
        expect(phase).toBe('terminal')
        expect(invoke).not.toHaveBeenCalledWith('peer_delta_target_foreground_release', { foreground })
    })

    test('current renderer preserves Running ownership when lost response never reaches Terminal', async () => {
        vi.useFakeTimers()
        try {
            const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
            let phase: 'absent' | 'running' = 'absent'
            const primary = new Error('native pull response lost')
            const bridge = { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) }
            const invoke = vi.fn(async <T>(command: string): Promise<T> => {
                if (command === 'peer_delta_target_foreground_status') {
                    return (phase === 'running' ? { foreground, phase } : null) as T
                }
                if (command === 'peer_delta_target_reserve') return foreground as T
                if (command === 'peer_delta_pull') {
                    phase = 'running'
                    throw primary
                }
                if (command === 'peer_delta_target_foreground_cancel') return true as T
                throw new Error(`unexpected ${command}`)
            }) as PeerDeltaInvoke
            const runtime: PeerDeltaMutationRuntime = {
                flushPendingData: vi.fn(async () => undefined),
                capturePersistentMutationToken: vi.fn(async () => ({ revision: 4, mutationGeneration: 2 })),
                acquireDestructiveReplacementFence: vi.fn(async () => ({
                    refreshCommittedWorkingSet: vi.fn(),
                    release: vi.fn(),
                })),
            }
            const facade = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

            const failurePromise = facade.pull(pairing).catch((error: unknown) => error)
            await vi.runAllTimersAsync()
            const failure = await failurePromise

            expect(failure).toBeInstanceOf(AggregateError)
            expect((failure as AggregateError).errors[0]).toBe(primary)
            expect((failure as AggregateError).errors[1]).toEqual(
                new Error('Android peer delta foreground cancellation timed out'),
            )
            expect(phase).toBe('running')
            expect(bridge.stopSource).not.toHaveBeenCalled()
            expect(invoke).not.toHaveBeenCalledWith('peer_delta_target_foreground_release', { foreground })
        } finally {
            vi.useRealTimers()
        }
    })

    test('keeps exact Android target cleanup retryable when Kotlin Stop returns false', async () => {
        const foreground = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
        let nativeOwner: typeof foreground | undefined
        const bridge = {
            startSource: vi.fn(() => true),
            stopSource: vi.fn().mockReturnValueOnce(false).mockReturnValueOnce(false).mockReturnValue(true),
        }
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 4, mutationGeneration: 2 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(async () => undefined),
                release: vi.fn(),
            })),
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_target_foreground_status') {
                return (nativeOwner ? {
                    foreground: nativeOwner,
                    phase: 'terminal',
                    result: { kind: 'updated', revision: 5, transferredObjects: 1, transferredBytes: 8 },
                } : null) as T
            }
            if (command === 'peer_delta_target_reserve') {
                if (nativeOwner) throw new Error('Android foreground service is already reserved')
                nativeOwner = foreground
                return foreground as T
            }
            if (command === 'peer_delta_target_foreground_release') {
                nativeOwner = undefined
                return true as T
            }
            return { kind: 'updated', revision: 5, transferredObjects: 1, transferredBytes: 8 } as T
        }) as PeerDeltaInvoke
        const first = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await expect(first.pull(pairing)).rejects.toThrow(
            'Android peer delta pull and foreground cleanup both failed',
        )
        expect(nativeOwner).toEqual(foreground)

        const reconstructed = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })
        await reconstructed.recoverTargetForeground()
        expect(nativeOwner).toBeUndefined()
        expect(bridge.stopSource).toHaveBeenCalledTimes(3)
    })

    test('recovers an exact Android target after a lost native release response before fresh reserve', async () => {
        const old = { lane: 'p4-target', operationId: '22222222-2222-4222-8222-222222222222', generation: 4 } as const
        const fresh = { lane: 'p4-target', operationId: '33333333-3333-4333-8333-333333333333', generation: 5 } as const
        let nativeOwner: typeof old | typeof fresh | undefined = old
        let loseReleaseResponse = true
        const bridge = { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) }
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 5, mutationGeneration: 3 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(async () => undefined),
                release: vi.fn(),
            })),
        }
        const invoke = vi.fn(async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
            if (command === 'peer_delta_target_foreground_status') return (nativeOwner ? { foreground: nativeOwner } : null) as T
            if (command === 'peer_delta_target_foreground_release') {
                if ((args?.foreground as typeof old).generation !== nativeOwner?.generation) return false as T
                nativeOwner = undefined
                if (loseReleaseResponse) {
                    loseReleaseResponse = false
                    throw new Error('invoke response lost')
                }
                return true as T
            }
            if (command === 'peer_delta_target_reserve') {
                if (nativeOwner) throw new Error('owner retained')
                nativeOwner = fresh
                return fresh as T
            }
            return { kind: 'noChanges', revision: 5, transferredObjects: 0, transferredBytes: 0 } as T
        }) as PeerDeltaInvoke
        const first = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })
        await expect(first.recoverTargetForeground()).rejects.toThrow('invoke response lost')

        const reconstructed = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })
        await reconstructed.recoverTargetForeground()
        await expect(reconstructed.pull(pairing)).resolves.toMatchObject({ kind: 'noChanges' })
        expect(nativeOwner).toBeUndefined()
    })

    test('reconstructed recovery cancels Running and waits for Terminal committed publication', async () => {
        const foreground = { lane: 'p4-target', operationId: '55555555-5555-4555-8555-555555555555', generation: 12 } as const
        const events: string[] = []
        let phase: 'running' | 'terminal' = 'running'
        const bridge = {
            startSource: vi.fn(() => true),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => { events.push('flush') }),
            capturePersistentMutationToken: vi.fn(async () => {
                events.push('capture')
                return { revision: 12, mutationGeneration: 4 }
            }),
            acquireDestructiveReplacementFence: vi.fn(async () => {
                events.push('fence')
                return {
                    async refreshCommittedWorkingSet(revision) { events.push(`refresh:${revision}`) },
                    release() { events.push('release') },
                }
            }),
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            events.push(command)
            if (command === 'peer_delta_target_foreground_status') {
                return {
                    foreground,
                    phase,
                    ...(phase === 'terminal' ? {
                        result: { kind: 'updated', revision: 13, transferredObjects: 1, transferredBytes: 32 },
                    } : {}),
                } as T
            }
            if (command === 'peer_delta_target_foreground_cancel') {
                phase = 'terminal'
                return true as T
            }
            if (command === 'peer_delta_target_foreground_release') return true as T
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const reconstructed = createPeerDeltaFacade({ platform: 'android', invoke, runtime, bridge })

        await reconstructed.recoverTargetForeground()

        expect(events).toEqual([
            'peer_delta_target_foreground_status',
            'peer_delta_target_foreground_cancel',
            'peer_delta_target_foreground_status',
            'flush', 'capture', 'fence', 'refresh:13', 'release',
            'service-stop', 'peer_delta_target_foreground_release',
        ])
    })

    test('Terminal precommit failure releases without refreshing data authority', async () => {
        const foreground = { lane: 'p4-target', operationId: '66666666-6666-4666-8666-666666666666', generation: 13 } as const
        const refresh = vi.fn()
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 12, mutationGeneration: 4 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({ refreshCommittedWorkingSet: refresh, release: vi.fn() })),
        }
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_target_foreground_status') {
                return { foreground, phase: 'terminal', error: 'cancelled before activation' } as T
            }
            if (command === 'peer_delta_target_foreground_release') return true as T
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({
            platform: 'android',
            invoke,
            runtime,
            bridge: { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) },
        })

        await facade.recoverTargetForeground()

        expect(refresh).not.toHaveBeenCalled()
        expect(runtime.flushPendingData).not.toHaveBeenCalled()
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
            async acquireDestructiveReplacementFence(token) {
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
            acquireDestructiveReplacementFence: vi.fn(async () => ({
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
            acquireDestructiveReplacementFence: vi.fn(async () => ({
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
            acquireDestructiveReplacementFence: vi.fn(async () => ({
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
        expect(runtime.acquireDestructiveReplacementFence).toHaveBeenCalledOnce()
        expect(refresh).toHaveBeenCalledTimes(2)
        expect(release).toHaveBeenCalledOnce()
    })

    test('retains Android Terminal cleanup ownership when committed refresh fails', async () => {
        const foreground = {
            lane: 'p4-target',
            operationId: '22222222-2222-4222-8222-222222222222',
            generation: 4,
        } as const
        const release = vi.fn()
        const refresh = vi.fn()
            .mockRejectedValueOnce(new Error('renderer refresh failed'))
            .mockResolvedValueOnce(undefined)
        const stopSource = vi.fn(() => true)
        const runtime: PeerDeltaMutationRuntime = {
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 8, mutationGeneration: 2 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
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
        let phase: 'absent' | 'terminal' = 'absent'
        let nativeReleaseCount = 0
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_target_foreground_status') {
                return (phase === 'terminal' ? { foreground, phase, result } : null) as T
            }
            if (command === 'peer_delta_target_reserve') return foreground as T
            if (command === 'peer_delta_pull') {
                phase = 'terminal'
                return result as T
            }
            if (command === 'peer_delta_target_foreground_release') {
                nativeReleaseCount += 1
                phase = 'absent'
                return true as T
            }
            throw new Error(`unexpected ${command}`)
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({
            platform: 'android',
            invoke,
            runtime,
            bridge: { startSource: vi.fn(() => true), stopSource },
        })

        await expect(facade.pull(pairing)).rejects.toThrow('renderer refresh failed')
        expect(refresh).toHaveBeenCalledOnce()
        expect(release).not.toHaveBeenCalled()
        expect(stopSource).not.toHaveBeenCalled()
        expect(invoke).not.toHaveBeenCalledWith('peer_delta_target_foreground_release', expect.anything())

        await expect(facade.pull(pairing)).resolves.toEqual(result)
        expect(refresh).toHaveBeenCalledTimes(2)
        expect(release).toHaveBeenCalledOnce()
        expect(stopSource).toHaveBeenCalledOnce()
        expect(stopSource).toHaveReturnedWith(true)
        expect(invoke).toHaveBeenCalledWith('peer_delta_target_foreground_release', { foreground })
        expect(nativeReleaseCount).toBe(1)
        expect(runtime.flushPendingData).toHaveBeenCalledOnce()
        expect(runtime.acquireDestructiveReplacementFence).toHaveBeenCalledOnce()
    })
    test('passes a well formed retained completion through and refuses any other shape', async () => {
        const retained = {
            operationId: '00000000-0000-4000-8000-000000000091',
            sourceDeviceId: '00000000-0000-4000-8000-000000000093',
            sourceName: 'Desk',
            witness: 'ambiguous',
            transferredObjects: 2,
            transferredBytes: 4096,
        }
        let response: unknown = retained
        const invoke = vi.fn(async <T>(command: string): Promise<T> => {
            if (command === 'peer_delta_target_retained') return response as T
            return undefined as T
        }) as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'desktop', invoke })

        await expect(facade.retained()).resolves.toEqual(retained)

        response = null
        await expect(facade.retained()).resolves.toBeNull()

        for (const invalid of [
            { ...retained, witness: 'unknown' },
            { ...retained, operationId: 'not-a-uuid' },
            { ...retained, transferredBytes: -1 },
            { ...retained, endpoint: 'http://192.168.0.9:32145' },
            'retained',
        ]) {
            response = invalid
            await expect(facade.retained()).rejects.toMatchObject({ code: 'state-unavailable' })
        }
    })

    test('abandons the retained completion by its exact operation identifier', async () => {
        const invoke = vi.fn(async () => undefined) as unknown as PeerDeltaInvoke
        const facade = createPeerDeltaFacade({ platform: 'android', invoke })

        await facade.abandonRetained('00000000-0000-4000-8000-000000000091')

        expect(invoke).toHaveBeenCalledWith('peer_delta_target_abandon', {
            operationId: '00000000-0000-4000-8000-000000000091',
        })
    })
})
