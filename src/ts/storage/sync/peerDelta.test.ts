import { describe, expect, test, vi } from 'vitest'

import {
    createPeerDeltaFacade,
    type PeerDeltaInvoke,
    type PeerDeltaMutationRuntime,
} from './peerDelta'

describe('peer logical delta target facade', () => {
    test('does not expose native delta on web', async () => {
        const platform = 'web' as const
        const facade = createPeerDeltaFacade({ platform })
        await expect(facade.capabilities()).rejects.toThrow(`unsupported on ${platform}`)
        await expect(facade.pullRegistered('source-device')).rejects.toThrow(`unsupported on ${platform}`)
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

        await facade.pullRegistered('source-device')

        expect(events).toEqual([
            'peer_delta_target_foreground_status', 'flush', 'capture', 'fence',
            'peer_delta_target_reserve', 'service-start', 'peer_delta_pull_registered',
            'refresh:5', 'release', 'service-stop', 'peer_delta_target_foreground_release',
        ])
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

        await expect(facade.pullRegistered('source-device')).rejects.toBe(primary)

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
            if (command === 'peer_delta_pull_registered') {
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

        await expect(facade.pullRegistered('source-device')).rejects.toBe(primary)

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
            if (command === 'peer_delta_pull_registered') {
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

        await expect(facade.pullRegistered('source-device')).rejects.toBe(primary)

        expect(events).toEqual([
            'peer_delta_target_foreground_status', 'flush', 'capture', 'fence',
            'peer_delta_target_reserve', 'peer_delta_pull_registered',
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
            if (command === 'peer_delta_pull_registered') {
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

        await expect(facade.pullRegistered('source-device')).rejects.toBe(primary)

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
            if (command === 'peer_delta_pull_registered') {
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

        const failure = await facade.pullRegistered('source-device').catch((error: unknown) => error)

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
                if (command === 'peer_delta_pull_registered') {
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

            const failurePromise = facade.pullRegistered('source-device').catch((error: unknown) => error)
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

        await expect(first.pullRegistered('source-device')).rejects.toThrow(
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
        await expect(reconstructed.pullRegistered('source-device')).resolves.toMatchObject({ kind: 'noChanges' })
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

        await expect(facade.pullRegistered('source-device')).resolves.toEqual({
            kind: 'updated',
            revision: 15,
            transferredObjects: 2,
            transferredBytes: 4096,
        })
        expect(events).toEqual([
            'flush',
            'capture',
            'fence:14:3',
            'invoke:peer_delta_pull_registered:14',
            'refresh:15',
            'release',
        ])
        expect(invoke).toHaveBeenCalledWith('peer_delta_pull_registered', {
            deviceId: 'source-device',
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

        await expect(facade.pullRegistered('source-device')).resolves.toEqual({
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

        await expect(facade.pullRegistered('source-device')).rejects.toThrow('stale revision')
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

        await expect(facade.pullRegistered('source-device')).rejects.toThrow('renderer refresh failed')
        expect(release).not.toHaveBeenCalled()

        await expect(facade.pullRegistered('source-device')).resolves.toEqual(result)
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
            if (command === 'peer_delta_pull_registered') {
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

        await expect(facade.pullRegistered('source-device')).rejects.toThrow('renderer refresh failed')
        expect(refresh).toHaveBeenCalledOnce()
        expect(release).not.toHaveBeenCalled()
        expect(stopSource).not.toHaveBeenCalled()
        expect(invoke).not.toHaveBeenCalledWith('peer_delta_target_foreground_release', expect.anything())

        await expect(facade.pullRegistered('source-device')).resolves.toEqual(result)
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
