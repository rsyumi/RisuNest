import { afterEach, describe, expect, it, vi } from 'vitest'

import { DeviceSyncError } from './deviceSync'
import { createDeviceSyncController, startDeviceSyncAutoListen } from './deviceSyncController'
import type { PeerCloneControllerSnapshot } from './peerCloneController'

const cloneSnapshot = (error = '') => ({
    sourceStatus: { phase: 'idle' as const, devices: [] },
    tunnelStatus: { phase: 'idle' as const },
    state: {
        source: { phase: 'idle' as const, revokedDeviceIds: [] },
        target: { phase: 'idle' as const, destructiveConfirmed: false, completedBytes: 0 },
    },
    sourcePairingUri: '', error, warning: '',
})

const deltaSnapshot = (error = '') => ({
    sourceStatus: { phase: 'idle' as const, devices: [] },
    tunnelStatus: { phase: 'idle' as const },
    sourcePairingUri: '', pullPhase: 'idle' as const, retained: null, error,
})

const bidirectionalSnapshot = (operationError = '') => ({
    sourceStatus: { phase: 'idle' as const, devices: [] }, sourcePairingUri: '',
    operationPhase: 'idle' as const, operationRetained: false, sourceBusy: false,
    sourceError: '', operationError,
})

const sourceFacade = (prepare = vi.fn(async () => ({ phase: 'prepared' as const }))) => ({
    status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
    prepare, start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
    rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
    revokeOutgoing: async () => undefined,
})

afterEach(() => {
    vi.useRealTimers()
})

describe('device sync controller', () => {
    it('retains clone backup paths in the safe target projection', () => {
        const clone = {
            ...cloneSnapshot(),
            state: {
                ...cloneSnapshot().state,
                target: {
                    ...cloneSnapshot().state.target,
                    phase: 'completed' as const,
                    backupPaths: ['C:\\sync\\pre-clone.lossless'],
                },
            },
        }
        const controller = createDeviceSyncController({
            facade: sourceFacade(),
            targets: {
                clone: {
                    snapshot: () => clone,
                    subscribe: (listener) => { listener(clone); return () => undefined },
                    initialize: async () => undefined,
                    joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(), download: vi.fn(),
                    resume: vi.fn(), cancel: vi.fn(),
                },
            },
        })

        expect(controller.snapshot().targets.clone?.state.target.backupPaths)
            .toEqual(['C:\\sync\\pre-clone.lossless'])
    })

    it('uses only incoming sources as receive targets and refreshes after a successful operation', async () => {
        const incomingSources = vi.fn(async () => [{ deviceId: 'incoming', name: 'Incoming', permissions: ['read'] as const }])
        const outgoingDevices = vi.fn(async () => [{ deviceId: 'outgoing', name: 'Outgoing', permissions: ['read'] as const }])
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources, outgoingDevices,
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
        })

        await controller.initialize()
        await controller.completeReceive(async () => undefined)

        expect(controller.snapshot().sources).toEqual([{ deviceId: 'incoming', name: 'Incoming', permissions: ['read'] }])
        expect(outgoingDevices).toHaveBeenCalledTimes(2)
        expect(incomingSources).toHaveBeenCalledTimes(2)
    })

    it('blocks receiving while the unified source is not off', async () => {
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'prepared' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
        })

        await controller.initialize()
        await expect(controller.completeReceive(async () => undefined)).rejects.toMatchObject({ code: 'unavailable' })
    })

    it('serializes source work and reports only a safe error', async () => {
        let release: (() => void) | undefined
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => await new Promise((resolve) => { release = () => resolve({ phase: 'prepared' as const }) }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => { throw new Error('http://private.example bearer secret') },
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
        })
        await controller.initialize()
        const preparing = controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })
        await expect(controller.start({ read: true, bidirectional: false }))
            .rejects.toMatchObject({ code: 'unavailable' })
        release?.()
        await preparing
        await expect(controller.rotateLink({ read: true, bidirectional: false })).rejects.toThrow('operation-failed')
        expect(controller.snapshot().error).toBe('operation-failed')
    })

    it('ignores an old poll resolution after stop publishes the authoritative idle state', async () => {
        vi.useFakeTimers()
        let resolveOld!: (value: { phase: 'running' }) => void
        const oldPoll = new Promise<{ phase: 'running' }>((resolve) => { resolveOld = resolve })
        const status = vi.fn()
            .mockResolvedValueOnce({ phase: 'idle' as const })
            .mockReturnValueOnce(oldPoll)
            .mockResolvedValueOnce({ phase: 'idle' as const })
        const controller = createDeviceSyncController({
            facade: {
                ...sourceFacade(), status,
                start: async () => ({ phase: 'running' as const }),
            },
            sourcePollMilliseconds: 10,
        })
        await controller.initialize()
        await controller.start({ read: true, bidirectional: false })
        await vi.advanceTimersByTimeAsync(10)
        expect(status).toHaveBeenCalledTimes(2)

        await controller.stop()
        resolveOld({ phase: 'running' })
        await vi.advanceTimersByTimeAsync(0)
        await vi.advanceTimersByTimeAsync(20)

        expect(controller.snapshot().source.phase).toBe('idle')
        expect(status).toHaveBeenCalledTimes(3)
    })

    it('ignores an old poll rejection after a later source lifecycle starts', async () => {
        vi.useFakeTimers()
        let rejectOld!: (error: Error) => void
        const oldPoll = new Promise<never>((_resolve, reject) => { rejectOld = reject })
        const status = vi.fn()
            .mockResolvedValueOnce({ phase: 'idle' as const })
            .mockReturnValueOnce(oldPoll)
            .mockResolvedValueOnce({ phase: 'idle' as const })
            .mockResolvedValue({ phase: 'running' as const })
        const controller = createDeviceSyncController({
            facade: {
                ...sourceFacade(), status,
                start: async () => ({ phase: 'running' as const }),
            },
            sourcePollMilliseconds: 10,
        })
        await controller.initialize()
        await controller.start({ read: true, bidirectional: false })
        await vi.advanceTimersByTimeAsync(10)
        await controller.stop()
        await controller.start({ read: true, bidirectional: false })

        rejectOld(new Error('stale private poll failure'))
        await vi.advanceTimersByTimeAsync(0)
        expect(controller.snapshot()).toMatchObject({ source: { phase: 'running' }, error: null })
        await vi.advanceTimersByTimeAsync(10)

        expect(controller.snapshot()).toMatchObject({ source: { phase: 'running' }, error: null })
        expect(status).toHaveBeenCalledTimes(4)
    })

    it('does no startup work when auto-listen is disabled', async () => {
        const calls: string[] = []
        const report = vi.fn()

        await startDeviceSyncAutoListen({
            syncAutoListen: false,
            syncListenMethod: 'fixed-url',
            syncFixedPort: 32145,
            syncPublicBaseUrl: 'https://sync.example.com',
        }, {
            controller: {
                initialize: async () => { calls.push('initialize') },
                prepare: async () => { calls.push('prepare'); return { phase: 'prepared' as const } },
                start: async () => { calls.push('start'); return { phase: 'running' as const } },
            },
            report,
        })

        expect(calls).toEqual([])
        expect(report).not.toHaveBeenCalled()
    })

    it('initializes, prepares the saved method, then starts auto-listen', async () => {
        const calls: string[] = []
        const report = vi.fn()
        await startDeviceSyncAutoListen({
            syncAutoListen: true,
            syncListenMethod: 'fixed-url',
            syncFixedPort: 32145,
            syncPublicBaseUrl: 'https://sync.example.com',
        }, {
            controller: {
                initialize: async () => { calls.push('initialize') },
                prepare: async (request) => { calls.push(`prepare:${request.method}`); return { phase: 'prepared' as const } },
                start: async () => { calls.push('start'); return { phase: 'running' as const } },
            },
            report,
        })

        expect(calls).toEqual(['initialize', 'prepare:fixed-url', 'start'])
        expect(report).not.toHaveBeenCalled()
    })

    it('reports a bounded automatic startup error after attempting the saved method', async () => {
        const start = vi.fn(async () => { throw new Error('private native failure') })
        const report = vi.fn()
        await startDeviceSyncAutoListen({
            syncAutoListen: true,
            syncListenMethod: 'quick',
            syncFixedPort: 32145,
            syncPublicBaseUrl: '',
        }, {
            controller: {
                initialize: async () => undefined,
                prepare: async () => ({ phase: 'prepared' as const }),
                start,
            },
            report,
        })

        expect(start).toHaveBeenCalledWith({ read: true, bidirectional: false })
        expect(report).toHaveBeenCalledOnce()
    })

    it('stages a v2 receipt without network work, then claims it when selected', async () => {
        const claimStagedClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const reconnectRegisteredClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const joinClaimed = vi.fn()
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }], outgoingDevices: async () => [],
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined, claimStagedClone, reconnectRegisteredClone,
            },
            targets: {
                clone: {
                    snapshot: () => cloneSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined,
                    joinClaimed, confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
                },
            },
        })
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)
        expect(claimStagedClone).not.toHaveBeenCalled()

        await controller.claimStagedClone()

        expect(claimStagedClone).toHaveBeenCalledOnce()
        expect(reconnectRegisteredClone).toHaveBeenCalledWith('source')
        expect(joinClaimed).toHaveBeenCalledOnce()
    })

    it('uses the staged source identity for a selected delta pull', async () => {
        const claimStagedClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const pullRegistered = vi.fn(async () => ({ kind: 'noChanges' }))
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined, claimStagedClone,
            },
            targets: { delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn() } },
        })
        await controller.initialize()
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)

        await controller.pullStagedDelta()

        expect(claimStagedClone).toHaveBeenCalledOnce()
        expect(pullRegistered).toHaveBeenCalledWith('source')
        expect(controller.snapshot().stagedLink).toBeNull()
    })

    it.each(['clone', 'delta', 'bidirectional'] as const)(
        'clears a previously staged %s source when its replacement link is invalid',
        async (lane) => {
            const claimStagedClone = vi.fn(async () => ({
                sourceDeviceId: 'source', endpoint: 'http://source/', sessionId: 'session', manifestId: 'a'.repeat(64),
            }))
            const joinRegistered = vi.fn(async () => undefined)
            const pullRegistered = vi.fn(async () => ({ kind: 'noChanges' }))
            const syncRegistered = vi.fn(async () => ({ kind: 'noChanges' }))
            const controller = createDeviceSyncController({
                facade: {
                    ...sourceFacade(),
                    incomingSources: async () => [{
                        deviceId: 'source', name: 'Source', permissions: ['read', 'bidirectional'] as const,
                    }],
                    claimStagedClone,
                },
                targets: {
                    clone: {
                        snapshot: () => cloneSnapshot(), subscribe: () => () => undefined,
                        initialize: async () => undefined, joinClaimed: vi.fn(), joinRegistered,
                        confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
                    },
                    delta: {
                        snapshot: () => deltaSnapshot(), subscribe: () => () => undefined,
                        initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn(),
                    },
                    bidirectional: {
                        snapshot: () => bidirectionalSnapshot(), subscribe: () => () => undefined,
                        initialize: async () => undefined, syncRegistered, resolveRegistered: vi.fn(),
                        resume: vi.fn(), acknowledge: vi.fn(), abandon: vi.fn(),
                    },
                },
            })
            await controller.initialize()
            const valid = `risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
            const run = lane === 'clone'
                ? () => controller.claimStagedClone()
                : lane === 'delta'
                    ? () => controller.pullStagedDelta()
                    : () => controller.syncStagedBidirectional()
            controller.stageLink(valid)
            await run()

            controller.stageLink('not a registration link')

            expect(controller.snapshot()).toMatchObject({
                stagedLink: null, stagedSourceDeviceId: null, error: 'operation-failed',
            })
            await expect(run()).rejects.toMatchObject({ code: 'unavailable' })
            expect(claimStagedClone).toHaveBeenCalledOnce()
            expect(joinRegistered).toHaveBeenCalledTimes(lane === 'clone' ? 1 : 0)
            expect(pullRegistered).toHaveBeenCalledTimes(lane === 'delta' ? 1 : 0)
            expect(syncRegistered).toHaveBeenCalledTimes(lane === 'bidirectional' ? 1 : 0)
        },
    )

    it.each(['clone', 'delta', 'bidirectional'] as const)(
        'fences an in-flight staged %s claim when an invalid link replaces it',
        async (lane) => {
            let resolveClaim!: (value: {
                sourceDeviceId: string, endpoint: string, sessionId: string, manifestId: string,
            }) => void
            const claimStagedClone = vi.fn(() => new Promise<{
                sourceDeviceId: string, endpoint: string, sessionId: string, manifestId: string,
            }>((resolve) => { resolveClaim = resolve }))
            const joinRegistered = vi.fn(async () => undefined)
            const pullRegistered = vi.fn(async () => ({ kind: 'noChanges' }))
            const syncRegistered = vi.fn(async () => ({ kind: 'noChanges' }))
            const controller = createDeviceSyncController({
                facade: {
                    ...sourceFacade(),
                    incomingSources: async () => [{
                        deviceId: 'old-source', name: 'Old', permissions: ['read', 'bidirectional'] as const,
                    }],
                    claimStagedClone,
                },
                targets: {
                    clone: {
                        snapshot: () => cloneSnapshot(), subscribe: () => () => undefined,
                        initialize: async () => undefined, joinClaimed: vi.fn(), joinRegistered,
                        confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
                    },
                    delta: {
                        snapshot: () => deltaSnapshot(), subscribe: () => () => undefined,
                        initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn(),
                    },
                    bidirectional: {
                        snapshot: () => bidirectionalSnapshot(), subscribe: () => () => undefined,
                        initialize: async () => undefined, syncRegistered, resolveRegistered: vi.fn(),
                        resume: vi.fn(), acknowledge: vi.fn(), abandon: vi.fn(),
                    },
                },
            })
            await controller.initialize()
            const valid = `risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
            const run = lane === 'clone'
                ? () => controller.claimStagedClone()
                : lane === 'delta'
                    ? () => controller.pullStagedDelta()
                    : () => controller.syncStagedBidirectional()
            controller.stageLink(valid)
            const pending = run()
            await vi.waitFor(() => expect(claimStagedClone).toHaveBeenCalledOnce())

            controller.stageLink('invalid replacement')
            resolveClaim({
                sourceDeviceId: 'old-source', endpoint: 'http://old/',
                sessionId: 'old-session', manifestId: 'a'.repeat(64),
            })

            await expect(pending).rejects.toMatchObject({ code: 'unavailable' })
            expect(controller.snapshot()).toMatchObject({ stagedLink: null, stagedSourceDeviceId: null })
            expect(joinRegistered).not.toHaveBeenCalled()
            expect(pullRegistered).not.toHaveBeenCalled()
            expect(syncRegistered).not.toHaveBeenCalled()
        },
    )

    it('fences a staged claim replaced while its registry refresh is pending', async () => {
        let resolveRefresh!: (value: Array<{
            deviceId: string, name: string, permissions: readonly ['read'],
        }>) => void
        const incomingSources = vi.fn()
            .mockResolvedValueOnce([{ deviceId: 'old-source', name: 'Old', permissions: ['read'] as const }])
            .mockImplementationOnce(() => new Promise((resolve) => { resolveRefresh = resolve }))
        const pullRegistered = vi.fn(async () => ({ kind: 'noChanges' }))
        const controller = createDeviceSyncController({
            facade: {
                ...sourceFacade(), incomingSources,
                claimStagedClone: async () => ({
                    sourceDeviceId: 'old-source', endpoint: 'http://old/',
                    sessionId: 'old-session', manifestId: 'a'.repeat(64),
                }),
            },
            targets: {
                delta: {
                    snapshot: () => deltaSnapshot(), subscribe: () => () => undefined,
                    initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn(),
                },
            },
        })
        await controller.initialize()
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)
        const pending = controller.pullStagedDelta()
        await vi.waitFor(() => expect(incomingSources).toHaveBeenCalledTimes(2))

        controller.stageLink('invalid replacement')
        resolveRefresh([{ deviceId: 'old-source', name: 'Old', permissions: ['read'] }])

        await expect(pending).rejects.toMatchObject({ code: 'unavailable' })
        expect(controller.snapshot()).toMatchObject({ stagedLink: null, stagedSourceDeviceId: null })
        expect(pullRegistered).not.toHaveBeenCalled()
    })

    it('initializes every target before completing unified initialization', async () => {
        const events: string[] = []
        const target = <T>(name: string, snapshot: T) => ({
            snapshot: () => snapshot,
            subscribe: (_listener: (value: T) => void) => () => undefined,
            initialize: vi.fn(async () => { events.push(name) }),
        })
        const clone = { ...target('clone', cloneSnapshot()), joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn() }
        const delta = { ...target('delta', deltaSnapshot()), pullRegistered: vi.fn(), abandonRetained: vi.fn() }
        const bidirectional = {
            ...target('bidirectional', bidirectionalSnapshot()), syncRegistered: vi.fn(), resolveRegistered: vi.fn(),
            resume: vi.fn(), acknowledge: vi.fn(), abandon: vi.fn(),
        }
        const controller = createDeviceSyncController({
            facade: {
                status: async () => { events.push('source'); return { phase: 'idle' as const } },
                incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
            targets: { clone, delta, bidirectional },
        })

        await controller.initialize()

        expect(events).toEqual(expect.arrayContaining(['clone', 'delta', 'bidirectional', 'source']))
        expect(clone.initialize).toHaveBeenCalledOnce()
        expect(delta.initialize).toHaveBeenCalledOnce()
        expect(bidirectional.initialize).toHaveBeenCalledOnce()
    })

    it('sanitizes all projected target errors', () => {
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
            targets: {
                clone: { snapshot: () => cloneSnapshot('bearer secret'), subscribe: (listener) => { listener(cloneSnapshot('bearer secret')); return () => undefined }, initialize: async () => undefined, joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn() },
                delta: { snapshot: () => deltaSnapshot('http://private'), subscribe: (listener) => { listener(deltaSnapshot('http://private')); return () => undefined }, initialize: async () => undefined, pullRegistered: vi.fn(), abandonRetained: vi.fn() },
                bidirectional: { snapshot: () => bidirectionalSnapshot('Authorization: secret'), subscribe: (listener) => { listener(bidirectionalSnapshot('Authorization: secret')); return () => undefined }, initialize: async () => undefined, syncRegistered: vi.fn(), resolveRegistered: vi.fn(), resume: vi.fn(), acknowledge: vi.fn(), abandon: vi.fn() },
            },
        })

        expect(controller.snapshot().targets.clone?.error).toBe('operation-failed')
        expect(controller.snapshot().targets.delta?.error).toBe('operation-failed')
        expect(controller.snapshot().targets.bidirectional?.operationError).toBe('operation-failed')
        expect(JSON.stringify(controller.snapshot())).not.toContain('secret')
        expect(JSON.stringify(controller.snapshot())).not.toContain('private')
    })

    it('consumes a staged claim once and retries transfer by retained source identity', async () => {
        const claimStagedClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://source/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const pullRegistered = vi.fn()
            .mockRejectedValueOnce(new Error('transfer failed after claim'))
            .mockResolvedValueOnce({ kind: 'noChanges' })
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined, claimStagedClone,
            },
            targets: { delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn() } },
        })
        await controller.initialize()
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)

        await expect(controller.pullStagedDelta()).rejects.toMatchObject({ code: 'operation-failed' })
        expect(controller.snapshot().stagedLink).toBeNull()
        expect(controller.snapshot().stagedSourceDeviceId).toBe('source')
        await expect(controller.pullStagedDelta()).resolves.toEqual({ kind: 'noChanges' })
        expect(claimStagedClone).toHaveBeenCalledOnce()
        expect(pullRegistered).toHaveBeenCalledTimes(2)
    })

    it('serializes source and target actions through one ownership gate', async () => {
        let release: (() => void) | undefined
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                outgoingDevices: async () => [],
                prepare: async () => await new Promise((resolve) => { release = () => resolve({ phase: 'prepared' as const }) }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined,
            },
            targets: { delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered: vi.fn(), abandonRetained: vi.fn() } },
        })
        await controller.initialize()
        const preparing = controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })

        await expect(controller.pullRegisteredDelta('source')).rejects.toBeInstanceOf(DeviceSyncError)
        release?.()
        await preparing
    })

    it.each(['downloading', 'cancelling', 'awaitingActivation', 'activating'] as const)(
        'blocks source preparation while a persistent clone target is %s',
        async (phase) => {
            const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
            const clone = {
                snapshot: () => ({
                    ...cloneSnapshot(),
                    targetPhase: phase,
                    state: {
                        ...cloneSnapshot().state,
                        target: {
                            ...cloneSnapshot().state.target,
                            phase: phase === 'downloading' ? 'downloading' as const : 'idle' as const,
                        },
                    },
                }),
                subscribe: () => () => undefined, initialize: async () => undefined, joinClaimed: vi.fn(),
                confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
            }
            const controller = createDeviceSyncController({ facade: sourceFacade(prepare), targets: { clone } })
            await controller.initialize()

            await expect(controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
                .rejects.toMatchObject({ code: 'unavailable' })
            expect(prepare).not.toHaveBeenCalled()
        },
    )

    it('blocks sharing after clone download start resolves while its persistent snapshot remains active', async () => {
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
        let value: PeerCloneControllerSnapshot = cloneSnapshot()
        const clone = {
            snapshot: () => value,
            subscribe: () => () => undefined, initialize: async () => undefined, joinClaimed: vi.fn(),
            confirmDestructiveReplace: vi.fn(),
            download: vi.fn(async () => {
                value = {
                    ...cloneSnapshot(),
                    state: {
                        ...cloneSnapshot().state,
                        target: { ...cloneSnapshot().state.target, phase: 'downloading' as const },
                    },
                }
            }),
            resume: vi.fn(), cancel: vi.fn(),
        }
        const controller = createDeviceSyncController({ facade: sourceFacade(prepare), targets: { clone } })
        await controller.initialize()
        await controller.downloadClone()

        await expect(controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toMatchObject({ code: 'unavailable' })
        expect(clone.download).toHaveBeenCalledOnce()
        expect(prepare).not.toHaveBeenCalled()
    })

    it('blocks source preparation for a recovered Android resumable clone job', async () => {
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
        const clone = {
            snapshot: () => ({ ...cloneSnapshot(), platform: 'android' as const, resumeAvailable: true }),
            subscribe: () => () => undefined, initialize: async () => undefined, joinClaimed: vi.fn(),
            confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
        }
        const controller = createDeviceSyncController({ facade: sourceFacade(prepare), targets: { clone } })
        await controller.initialize()

        await expect(controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toMatchObject({ code: 'unavailable' })
        expect(prepare).not.toHaveBeenCalled()
    })

    it('blocks source preparation after a delta pull promise resolves while its snapshot remains running', async () => {
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
        const delta = {
            snapshot: () => ({ ...deltaSnapshot(), pullPhase: 'running' as const }),
            subscribe: () => () => undefined, initialize: async () => undefined,
            pullRegistered: vi.fn(async () => ({ kind: 'noChanges' })), abandonRetained: vi.fn(),
        }
        const controller = createDeviceSyncController({ facade: sourceFacade(prepare), targets: { delta } })
        await controller.initialize()

        await expect(controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toMatchObject({ code: 'unavailable' })
        expect(prepare).not.toHaveBeenCalled()
    })

    it.each([
        ['running', false],
        ['awaitingConflict', false],
        ['sourcePrepared', true],
        ['targetPrepared', false],
        ['localCommitted', false],
        ['sourceUnavailable', false],
        ['refreshPending', false],
        ['completed', true],
        ['stale', true],
    ] as const)('applies the source rehost rule to durable bidirectional phase %s', async (operationPhase, allowed) => {
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
        const bidirectional = {
            snapshot: () => ({
                ...bidirectionalSnapshot(), operationPhase,
                operationRetained: !['stale'].includes(operationPhase),
            }),
            subscribe: () => () => undefined, initialize: async () => undefined,
            syncRegistered: vi.fn(), resolveRegistered: vi.fn(), resume: vi.fn(), acknowledge: vi.fn(), abandon: vi.fn(),
        }
        const controller = createDeviceSyncController({ facade: sourceFacade(prepare), targets: { bidirectional } })
        await controller.initialize()
        const preparing = controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })

        if (allowed) await expect(preparing).resolves.toMatchObject({ phase: 'prepared' })
        else await expect(preparing).rejects.toMatchObject({ code: 'unavailable' })
        expect(prepare).toHaveBeenCalledTimes(allowed ? 1 : 0)
    })

    it('prevents auto-listen from sharing while a recovered target is active', async () => {
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
        const start = vi.fn(async () => ({ phase: 'running' as const }))
        const clone = {
            snapshot: () => ({
                ...cloneSnapshot(),
                state: { ...cloneSnapshot().state, target: { ...cloneSnapshot().state.target, phase: 'downloading' as const } },
            }),
            subscribe: () => () => undefined, initialize: async () => undefined, joinClaimed: vi.fn(),
            confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
        }
        const controller = createDeviceSyncController({
            facade: { ...sourceFacade(prepare), start }, targets: { clone },
        })
        const report = vi.fn()

        await startDeviceSyncAutoListen({
            syncAutoListen: true, syncListenMethod: 'lan', syncFixedPort: 32145, syncPublicBaseUrl: '',
        }, { controller, report })

        expect(prepare).not.toHaveBeenCalled()
        expect(start).not.toHaveBeenCalled()
        expect(report).toHaveBeenCalledWith(expect.objectContaining({ code: 'unavailable' }))
    })

    it('blocks receive work while the native source is preparing', async () => {
        const pullRegistered = vi.fn()
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'preparing' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined,
            },
            targets: {
                delta: {
                    snapshot: () => deltaSnapshot(), subscribe: () => () => undefined,
                    initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn(),
                },
            },
        })
        await controller.initialize()

        await expect(controller.pullRegisteredDelta('source')).rejects.toMatchObject({ code: 'unavailable' })
        expect(pullRegistered).not.toHaveBeenCalled()
    })

    it('marks 401 registrations expired in memory and clears them after re-registration', async () => {
        const pullRegistered = vi.fn(async () => { throw new DeviceSyncError('registration-expired') })
        const claimStagedClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://source/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const reconnectRegisteredClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://source/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const clone = {
            snapshot: () => cloneSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined,
            joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
        }
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined, claimStagedClone, reconnectRegisteredClone,
            },
            targets: {
                clone,
                delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn() },
            },
        })
        await controller.initialize()
        await expect(controller.pullRegisteredDelta('source')).rejects.toMatchObject({ code: 'registration-expired' })
        expect(controller.snapshot().expiredSourceIds).toEqual(['source'])
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)
        await controller.claimStagedClone()
        expect(controller.snapshot().expiredSourceIds).toEqual([])
        expect(reconnectRegisteredClone).toHaveBeenCalledWith('source')
    })

    it('keeps a registered source retryable after a temporary hello transport failure', async () => {
        const pullRegistered = vi.fn()
            .mockRejectedValueOnce(new Error('transportUnavailable'))
            .mockResolvedValueOnce({ kind: 'noChanges' })
        const controller = createDeviceSyncController({
            facade: {
                ...sourceFacade(),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
            },
            targets: {
                delta: {
                    snapshot: () => deltaSnapshot(), subscribe: () => () => undefined,
                    initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn(),
                },
            },
        })
        await controller.initialize()

        await expect(controller.pullRegisteredDelta('source'))
            .rejects.toMatchObject({ code: 'transport-unavailable' })
        expect(controller.snapshot().expiredSourceIds).toEqual([])

        await expect(controller.pullRegisteredDelta('source')).resolves.toEqual({ kind: 'noChanges' })
        expect(pullRegistered).toHaveBeenCalledTimes(2)
        expect(controller.snapshot().expiredSourceIds).toEqual([])
    })

    it('marks an authenticated source identity change for re-registration', async () => {
        const pullRegistered = vi.fn(async () => { throw new Error('identityMismatch') })
        const controller = createDeviceSyncController({
            facade: {
                ...sourceFacade(),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
            },
            targets: {
                delta: {
                    snapshot: () => deltaSnapshot(), subscribe: () => () => undefined,
                    initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn(),
                },
            },
        })
        await controller.initialize()

        await expect(controller.pullRegisteredDelta('source'))
            .rejects.toMatchObject({ code: 'transport-changed' })
        expect(controller.snapshot().expiredSourceIds).toEqual(['source'])
    })

    it.each(['downloadClone', 'resumeClone', 'cancelClone'] as const)(
        'attributes %s expiry to the selected clone source, never a stale staged source',
        async (action) => {
            const clone = {
                snapshot: () => cloneSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined,
                joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(),
                download: vi.fn(async () => { throw new DeviceSyncError('registration-expired') }),
                resume: vi.fn(async () => { throw new DeviceSyncError('registration-expired') }),
                cancel: vi.fn(async () => { throw new DeviceSyncError('registration-expired') }),
            }
            const controller = createDeviceSyncController({
                facade: {
                    status: async () => ({ phase: 'idle' as const }),
                    incomingSources: async () => [
                        { deviceId: 'staged-source', name: 'Staged', permissions: ['read'] as const },
                        { deviceId: 'selected-source', name: 'Selected', permissions: ['read'] as const },
                    ],
                    outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                    start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                    rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                    revokeOutgoing: async () => undefined,
                    claimStagedClone: async () => ({
                        sourceDeviceId: 'staged-source', endpoint: 'http://source/',
                        sessionId: 'session', manifestId: 'a'.repeat(64),
                    }),
                    reconnectRegisteredClone: async (deviceId) => ({
                        sourceDeviceId: deviceId, endpoint: 'http://source/',
                        sessionId: 'session', manifestId: 'a'.repeat(64),
                    }),
                },
                targets: { clone },
            })
            await controller.initialize()
            controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)
            await controller.claimStagedClone()
            await controller.selectRegisteredClone('selected-source')

            await expect(controller[action]()).rejects.toMatchObject({ code: 'registration-expired' })
            expect(controller.snapshot().expiredSourceIds).toEqual(['selected-source'])
            expect(controller.snapshot().activeCloneSourceDeviceId).toBe('selected-source')
            expect(controller.snapshot().stagedSourceDeviceId).toBe('staged-source')
        },
    )

    it('attributes durable bidirectional retry and resolve expiry to their active registered source', async () => {
        const resume = vi.fn(async () => { throw new DeviceSyncError('registration-expired') })
        const resolveRegistered = vi.fn(async () => { throw new DeviceSyncError('registration-expired') })
        const bidirectional = {
            snapshot: () => bidirectionalSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined,
            syncRegistered: vi.fn(async () => ({ kind: 'conflict' })), resolveRegistered,
            resume, acknowledge: vi.fn(), abandon: vi.fn(),
        }
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [
                    { deviceId: 'staged-source', name: 'Staged', permissions: ['read', 'bidirectional'] as const },
                    { deviceId: 'bidi-source', name: 'Bidi', permissions: ['read', 'bidirectional'] as const },
                    { deviceId: 'resolve-source', name: 'Resolve', permissions: ['read', 'bidirectional'] as const },
                ],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined,
                claimStagedClone: async () => ({
                    sourceDeviceId: 'staged-source', endpoint: 'http://source/',
                    sessionId: 'session', manifestId: 'a'.repeat(64),
                }),
            },
            targets: { bidirectional },
        })
        await controller.initialize()
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)
        await controller.syncStagedBidirectional()
        await controller.syncRegisteredBidirectional('bidi-source')

        await expect(controller.resumeBidirectional()).rejects.toMatchObject({ code: 'registration-expired' })
        expect(controller.snapshot().expiredSourceIds).toEqual(['bidi-source'])
        expect(controller.snapshot().stagedSourceDeviceId).toBe('staged-source')
        await expect(controller.resolveRegisteredBidirectional('resolve-source', 'local'))
            .rejects.toMatchObject({ code: 'registration-expired' })
        expect(controller.snapshot().expiredSourceIds).toEqual(['bidi-source', 'resolve-source'])
        expect(controller.snapshot().activeBidirectionalSourceDeviceId).toBe('resolve-source')
    })

    it('clears clone expiry when that exact source is re-registered successfully', async () => {
        let reconnectFails = true
        const clone = {
            snapshot: () => cloneSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined,
            joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn(),
        }
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined,
                reconnectRegisteredClone: async () => {
                    if (reconnectFails) throw new DeviceSyncError('registration-expired')
                    return { sourceDeviceId: 'source', endpoint: 'http://source/', sessionId: 'session', manifestId: 'a'.repeat(64) }
                },
            },
            targets: { clone },
        })
        await controller.initialize()
        await expect(controller.selectRegisteredClone('source')).rejects.toMatchObject({ code: 'registration-expired' })
        reconnectFails = false

        await controller.selectRegisteredClone('source')

        expect(controller.snapshot().expiredSourceIds).toEqual([])
        expect(controller.snapshot().activeCloneSourceDeviceId).toBe('source')
    })

    it('exposes the complete typed target action surface through one controller', async () => {
        const clone = {
            snapshot: () => cloneSnapshot(), subscribe: () => () => undefined, initialize: vi.fn(async () => undefined),
            joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(), download: vi.fn(async () => undefined),
            resume: vi.fn(async () => undefined), cancel: vi.fn(async () => undefined),
        }
        const delta = {
            snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: vi.fn(async () => undefined),
            pullRegistered: vi.fn(async () => ({ kind: 'noChanges' })), abandonRetained: vi.fn(),
        }
        const bidirectional = {
            snapshot: () => bidirectionalSnapshot(), subscribe: () => () => undefined, initialize: vi.fn(async () => undefined),
            syncRegistered: vi.fn(async () => ({ kind: 'noChanges' })), resolveRegistered: vi.fn(async () => ({ kind: 'updated' })),
            resume: vi.fn(async () => ({ kind: 'updated' })), acknowledge: vi.fn(async () => undefined), abandon: vi.fn(async () => undefined),
        }
        const reconnectRegisteredClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://source/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read', 'bidirectional'] as const }],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined, reconnectRegisteredClone,
            },
            targets: { clone, delta, bidirectional },
        })
        await controller.initialize()

        await controller.selectRegisteredClone('source')
        await controller.confirmCloneReplace()
        await controller.downloadClone()
        await controller.resumeClone()
        await controller.cancelClone()
        await controller.pullRegisteredDelta('source')
        await controller.syncRegisteredBidirectional('source')
        await controller.resolveRegisteredBidirectional('source', 'remote')
        await controller.resumeBidirectional()
        await controller.acknowledgeBidirectional()
        await controller.abandonBidirectional()

        expect(clone.joinClaimed).toHaveBeenCalledOnce()
        expect(clone.confirmDestructiveReplace).toHaveBeenCalledOnce()
        expect(clone.download).toHaveBeenCalledOnce()
        expect(clone.resume).toHaveBeenCalledOnce()
        expect(clone.cancel).toHaveBeenCalledOnce()
        expect(delta.pullRegistered).toHaveBeenCalledWith('source')
        expect(bidirectional.syncRegistered).toHaveBeenCalledWith('source')
        expect(bidirectional.resolveRegistered).toHaveBeenCalledWith('source', 'remote')
        expect(bidirectional.resume).toHaveBeenCalledOnce()
        expect(bidirectional.acknowledge).toHaveBeenCalledOnce()
        expect(bidirectional.abandon).toHaveBeenCalledOnce()
    })

    it('owns pending v2 receipt subscription and disposes it', () => {
        const subscribe = vi.fn((listener: (uri: string) => void) => {
            listener(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)
            return vi.fn()
        })
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
            deepLinks: { consumePending: () => null, subscribe },
        })
        expect(controller.snapshot().stagedLink).not.toBeNull()
        const unsubscribe = subscribe.mock.results[0].value

        controller.dispose()

        expect(unsubscribe).toHaveBeenCalledOnce()
    })

    it('owns the raw pending link and retries a post-claim failure without claiming again', async () => {
        const uri = `risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
        const claimStagedClone = vi.fn(async () => ({
            sourceDeviceId: 'source', endpoint: 'http://192.168.1.2:32145/',
            sessionId: 'session', manifestId: 'a'.repeat(64),
        }))
        const pullRegistered = vi.fn()
            .mockRejectedValueOnce(new Error('transfer failed after claim'))
            .mockResolvedValueOnce({ kind: 'noChanges' })
        const controller = createDeviceSyncController({
            facade: {
                ...sourceFacade(),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                claimStagedClone,
            },
            targets: {
                delta: {
                    snapshot: () => deltaSnapshot(), subscribe: () => () => undefined,
                    initialize: async () => undefined, pullRegistered, abandonRetained: vi.fn(),
                },
            },
            deepLinks: { consumePending: () => uri, subscribe: () => () => undefined },
        })

        expect(controller.snapshot().stagedUri).toBe(uri)
        await expect(controller.pullStagedDelta()).rejects.toMatchObject({ code: 'operation-failed' })
        expect(controller.snapshot()).toMatchObject({ stagedUri: null, stagedLink: null, stagedSourceDeviceId: 'source' })

        await controller.pullStagedDelta()

        expect(claimStagedClone).toHaveBeenCalledOnce()
        expect(pullRegistered).toHaveBeenCalledTimes(2)
    })

    it.each([
        ['idle', ['prepare', 'start']],
        ['prepared', ['start']],
    ] as const)('rehosts a sourcePrepared operation from %s without resuming the target', async (phase, expected) => {
        const calls: string[] = []
        const prepare = vi.fn(async () => { calls.push('prepare'); return { phase: 'prepared' as const } })
        const start = vi.fn(async () => { calls.push('start'); return { phase: 'running' as const } })
        const resume = vi.fn(async () => { calls.push('resume') })
        const bidirectional = {
            snapshot: () => ({ ...bidirectionalSnapshot(), operationPhase: 'sourcePrepared' as const, operationRetained: true }),
            subscribe: () => () => undefined, initialize: async () => undefined,
            syncRegistered: vi.fn(), resolveRegistered: vi.fn(), resume,
            acknowledge: vi.fn(), abandon: vi.fn(),
        }
        const controller = createDeviceSyncController({
            facade: { ...sourceFacade(prepare), status: async () => ({ phase }), start },
            targets: { bidirectional },
        })
        await controller.initialize()

        await controller.rehostBidirectionalSource(
            { method: 'lan', fixedPort: 32145, publicBaseUrl: '' },
            { read: true, bidirectional: false },
        )

        expect(calls).toEqual(expected)
        expect(resume).not.toHaveBeenCalled()
        expect(controller.snapshot().source.phase).toBe('running')
    })

    it('retains the prepared source after rehost start fails so retry does not prepare twice', async () => {
        vi.useFakeTimers()
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const }))
        const start = vi.fn()
            .mockRejectedValueOnce(new Error('start failed'))
            .mockResolvedValueOnce({ phase: 'running' as const })
        const bidirectional = {
            snapshot: () => ({ ...bidirectionalSnapshot(), operationPhase: 'sourcePrepared' as const, operationRetained: true }),
            subscribe: () => () => undefined, initialize: async () => undefined,
            syncRegistered: vi.fn(), resolveRegistered: vi.fn(), resume: vi.fn(),
            acknowledge: vi.fn(), abandon: vi.fn(),
        }
        const controller = createDeviceSyncController({
            facade: {
                ...sourceFacade(prepare), start,
                status: async () => ({ phase: 'error' as const, latestError: 'transport-unavailable' as const }),
            },
            targets: { bidirectional },
            sourcePollMilliseconds: 10,
        })
        const settings = { method: 'lan' as const, fixedPort: 32145, publicBaseUrl: '' }
        const permissions = { read: true, bidirectional: false }

        await expect(controller.rehostBidirectionalSource(settings, permissions))
            .rejects.toMatchObject({ code: 'operation-failed' })
        expect(controller.snapshot().source.phase).toBe('prepared')
        await vi.advanceTimersByTimeAsync(10)
        expect(controller.snapshot().source.phase).toBe('error')

        await controller.rehostBidirectionalSource(settings, permissions)

        expect(prepare).toHaveBeenCalledOnce()
        expect(start).toHaveBeenCalledTimes(2)
    })

    it('keeps source and receive errors in distinct snapshot scopes', async () => {
        const controller = createDeviceSyncController({
            facade: sourceFacade(vi.fn(async () => { throw new Error('private source failure') })),
        })

        await expect(controller.completeReceive(async () => { throw new Error('private receive failure') }))
            .rejects.toMatchObject({ code: 'operation-failed' })
        expect(controller.snapshot()).toMatchObject({ sourceError: null, workError: 'operation-failed' })

        await expect(controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toMatchObject({ code: 'operation-failed' })
        expect(controller.snapshot()).toMatchObject({ sourceError: 'operation-failed', workError: 'operation-failed' })
    })
    it('passes the retained delta completion through and delegates abandoning it', async () => {
        const retained = {
            operationId: '00000000-0000-4000-8000-000000000091',
            sourceDeviceId: '00000000-0000-4000-8000-000000000093',
            sourceName: 'Desk',
            witness: 'ambiguous' as const,
            transferredObjects: 2,
            transferredBytes: 4096,
        }
        const withRetained = { ...deltaSnapshot(), retained }
        const abandonRetained = vi.fn(async () => undefined)
        const controller = createDeviceSyncController({
            facade: sourceFacade(),
            targets: {
                delta: {
                    snapshot: () => withRetained,
                    subscribe: (listener) => { listener(withRetained); return () => undefined },
                    initialize: async () => undefined,
                    pullRegistered: vi.fn(), abandonRetained,
                },
            },
        })

        // The device name and the transfer size carry no credential, so they
        // reach the page unchanged.
        expect(controller.snapshot().targets.delta?.retained).toEqual(retained)

        await controller.abandonDelta()

        expect(abandonRetained).toHaveBeenCalledOnce()
        expect(controller.snapshot().workError).toBeNull()
    })

    it('reports an abandoned delta failure as a safe work error', async () => {
        const controller = createDeviceSyncController({
            facade: sourceFacade(),
            targets: {
                delta: {
                    snapshot: () => deltaSnapshot(),
                    subscribe: () => () => undefined,
                    initialize: async () => undefined,
                    pullRegistered: vi.fn(),
                    abandonRetained: vi.fn(async () => { throw new Error('http://private.example bearer secret') }),
                },
            },
        })

        await expect(controller.abandonDelta()).rejects.toMatchObject({ code: 'operation-failed' })
        expect(controller.snapshot().workError).toBe('operation-failed')
    })
})
