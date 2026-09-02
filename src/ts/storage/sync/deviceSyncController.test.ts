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
    sourcePairingUri: '', pullPhase: 'idle' as const, error,
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

    it('starts the saved method without surfacing an automatic startup error', async () => {
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
            targets: { delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered } },
        })
        await controller.initialize()
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)

        await controller.pullStagedDelta()

        expect(claimStagedClone).toHaveBeenCalledOnce()
        expect(pullRegistered).toHaveBeenCalledWith('source')
        expect(controller.snapshot().stagedLink).toBeNull()
    })

    it('initializes every target before completing unified initialization', async () => {
        const events: string[] = []
        const target = <T>(name: string, snapshot: T) => ({
            snapshot: () => snapshot,
            subscribe: (_listener: (value: T) => void) => () => undefined,
            initialize: vi.fn(async () => { events.push(name) }),
        })
        const clone = { ...target('clone', cloneSnapshot()), joinClaimed: vi.fn(), confirmDestructiveReplace: vi.fn(), download: vi.fn(), resume: vi.fn(), cancel: vi.fn() }
        const delta = { ...target('delta', deltaSnapshot()), pullRegistered: vi.fn() }
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
                delta: { snapshot: () => deltaSnapshot('http://private'), subscribe: (listener) => { listener(deltaSnapshot('http://private')); return () => undefined }, initialize: async () => undefined, pullRegistered: vi.fn() },
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
            targets: { delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered } },
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
            targets: { delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered: vi.fn() } },
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
            pullRegistered: vi.fn(async () => ({ kind: 'noChanges' })),
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
                    initialize: async () => undefined, pullRegistered,
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
                delta: { snapshot: () => deltaSnapshot(), subscribe: () => () => undefined, initialize: async () => undefined, pullRegistered },
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
            pullRegistered: vi.fn(async () => ({ kind: 'noChanges' })),
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
})
