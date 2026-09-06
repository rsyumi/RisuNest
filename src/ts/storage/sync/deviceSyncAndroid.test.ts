import { describe, expect, it, vi } from 'vitest'

import { createDeviceSyncController } from './deviceSyncController'
import { createAndroidDeviceSyncFacade } from './deviceSyncAndroid'

const foreground = {
    lane: 'device-sync-source' as const,
    operationId: '11111111-1111-4111-8111-111111111111',
    generation: 7,
}

const runtimeStub = (flushPendingData?: (reason: string) => Promise<void>) => {
    const fence = {
        refreshCommittedWorkingSet: vi.fn(async (_revision: number) => undefined),
        release: vi.fn(),
    }
    return {
        fence,
        flushPendingData: vi.fn(flushPendingData ?? (async (_reason: string) => undefined)),
        capturePersistentMutationToken: vi.fn(async (_reason: string) => ({ revision: 1, mutationGeneration: 1 })),
        acquireDestructiveReplacementFence: vi.fn(async (_token: {
            revision: number
            mutationGeneration: number
        }) => fence),
    }
}

function fixture(options: {
    startSource?: () => boolean
    stopSource?: () => boolean
    flushPendingData?: (reason: string) => Promise<void>
} = {}) {
    const calls: Array<[string, Record<string, unknown> | undefined]> = []
    const invoke = vi.fn(async (command: string, args?: Record<string, unknown>): Promise<unknown> => {
        calls.push([command, args])
        if (command === 'peer_sync_foreground_source_status') return null
        if (command === 'device_sync_source_reserve') return foreground
        if (command === 'device_sync_start') return { phase: 'running' }
        if (command === 'device_sync_stop') return foreground
        if (command === 'device_sync_status') return { phase: 'idle' }
        if (command === 'peer_sync_foreground_source_abandon') return true
        return { phase: 'prepared' }
    })
    const bridge = {
        startSource: vi.fn(options.startSource ?? (() => true)),
        stopSource: vi.fn(options.stopSource ?? (() => true)),
    }
    const runtime = runtimeStub(options.flushPendingData)
    return {
        calls,
        invoke,
        bridge,
        runtime,
        facade: createAndroidDeviceSyncFacade({ invoke, bridge, runtime }),
    }
}

describe('Android unified device sync source facade', () => {
    it('forwards the LAN port without a saved public URL and rejects tunnel methods before native invocation', async () => {
        const { facade, invoke } = fixture()

        await facade.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: 'https://saved.example' })
        await expect(facade.prepare({ method: 'quick', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toMatchObject({ code: 'invalid-configuration' })
        await expect(facade.prepare({ method: 'fixed-url', fixedPort: 32145, publicBaseUrl: 'https://sync.example' }))
            .rejects.toMatchObject({ code: 'invalid-configuration' })

        expect(invoke.mock.calls.filter(([command]) => command === 'device_sync_prepare')).toEqual([[
            'device_sync_prepare',
            { request: { method: 'lan', fixedPort: 32145, publicBaseUrl: '' } },
        ]])
    })

    it('awaits the persistent-data flush before native LAN preparation', async () => {
        const events: string[] = []
        let finishFlush: (() => void) | undefined
        const { facade, invoke, runtime } = fixture({
            flushPendingData: async () => {
                events.push('flush-start')
                await new Promise<void>((resolve) => { finishFlush = resolve })
                events.push('flush-finish')
            },
        })
        invoke.mockImplementation(async (command: string) => {
            events.push(command)
            return { phase: 'prepared' }
        })

        const preparing = facade.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })
        await vi.waitFor(() => expect(events).toEqual(['flush-start']))
        expect(invoke).not.toHaveBeenCalled()

        finishFlush?.()
        await preparing

        expect(runtime.flushPendingData).toHaveBeenCalledWith('device-sync-source-prepare')
        expect(runtime.flushPendingData).toHaveBeenCalledOnce()
        expect(events).toEqual(['flush-start', 'flush-finish', 'device_sync_prepare'])
    })

    it('does not invoke native preparation when the persistent-data flush fails', async () => {
        const flushFailure = new Error('flush failed')
        const { facade, invoke } = fixture({
            flushPendingData: async () => { throw flushFailure },
        })

        await expect(facade.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toBe(flushFailure)

        expect(invoke).not.toHaveBeenCalled()
    })

    it('omits the desktop registered clone reconnect that Android answers with another shape', () => {
        const { facade } = fixture()

        expect('reconnectRegisteredClone' in facade).toBe(false)
        expect(createDeviceSyncController({ facade }).snapshot().error).toBeNull()
    })

    it('exposes the shared remote commit refresh on the same mutation runtime', async () => {
        const { facade, runtime } = fixture()

        await expect(facade.refreshAfterRemoteCommit({
            operationId: '00000000-0000-4000-8000-0000000000a1', committedRevision: 12,
        })).resolves.toEqual({ discardedPendingEdits: false })

        expect(runtime.flushPendingData).toHaveBeenCalledWith('device-sync-remote-commit')
        expect(runtime.fence.refreshCommittedWorkingSet).toHaveBeenCalledWith(12)
        expect(runtime.fence.release).toHaveBeenCalledOnce()
    })

    it('recovers once, reserves once, and attaches the exact foreground identity without secrets', async () => {
        const { facade, calls, bridge } = fixture()
        const permissions = { read: true, bidirectional: false }

        await expect(facade.start(permissions)).resolves.toEqual({ phase: 'running' })

        expect(calls).toEqual([
            ['peer_sync_foreground_source_status', undefined],
            ['device_sync_source_reserve', undefined],
            ['device_sync_start', { permissions, foreground }],
        ])
        expect(bridge.startSource).toHaveBeenCalledOnce()
        expect(bridge.startSource).toHaveBeenCalledWith(
            'device-sync-source', foreground.operationId, foreground.generation,
        )
        expect(JSON.stringify(calls)).not.toMatch(/endpoint|bearer|token|claim/i)
    })

    it('stops and abandons a stale exact generation before reserving a new source', async () => {
        const stale = { ...foreground, generation: 6 }
        const { facade, invoke, bridge } = fixture()
        invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'peer_sync_foreground_source_status') return stale
            if (command === 'peer_sync_foreground_source_abandon') return true
            if (command === 'device_sync_source_reserve') return foreground
            if (command === 'device_sync_start') return { phase: 'running' }
            throw new Error(`Unexpected command: ${command} ${JSON.stringify(args)}`)
        })

        await facade.start({ read: true, bidirectional: true })

        expect(bridge.stopSource).toHaveBeenCalledWith(
            'device-sync-source', stale.operationId, stale.generation,
        )
        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'peer_sync_foreground_source_status',
            'peer_sync_foreground_source_abandon',
            'device_sync_source_reserve',
            'device_sync_start',
        ])
    })

    it('rejects secret-bearing recovered foreground state before touching the bridge', async () => {
        const { facade, invoke, bridge } = fixture()
        invoke.mockImplementation(async (command: string) => {
            if (command === 'peer_sync_foreground_source_status') {
                return { ...foreground, bearer: 'native secret' }
            }
            throw new Error(`Unexpected command: ${command}`)
        })

        await expect(facade.start({ read: true, bidirectional: false }))
            .rejects.toMatchObject({ code: 'state-unavailable' })

        expect(bridge.startSource).not.toHaveBeenCalled()
        expect(bridge.stopSource).not.toHaveBeenCalled()
    })

    it.each([
        ['non-UUID operation', { ...foreground, operationId: 'not-a-uuid' }],
        ['zero generation', { ...foreground, generation: 0 }],
    ])('rejects recovered foreground state with %s', async (_label, invalidForeground) => {
        const { facade, invoke, bridge } = fixture()
        invoke.mockImplementation(async (command: string) => {
            if (command === 'peer_sync_foreground_source_status') return invalidForeground
            throw new Error(`Unexpected command: ${command}`)
        })

        await expect(facade.start({ read: true, bidirectional: false }))
            .rejects.toMatchObject({ code: 'state-unavailable' })
        expect(bridge.startSource).not.toHaveBeenCalled()
        expect(bridge.stopSource).not.toHaveBeenCalled()
    })

    it('preserves native start and cleanup failures for the controller sanitizer', async () => {
        const primary = new Error('private native attach detail')
        const cleanup = new Error('private abandon detail')
        const { facade, invoke } = fixture()
        invoke.mockImplementation(async (command: string) => {
            if (command === 'peer_sync_foreground_source_status') return null
            if (command === 'device_sync_source_reserve') return foreground
            if (command === 'device_sync_start') throw primary
            if (command === 'peer_sync_foreground_source_abandon') throw cleanup
            if (command === 'device_sync_status') return { phase: 'idle' }
            if (command === 'peer_sync_incoming_sources' || command === 'peer_sync_outgoing_devices') return []
            throw new Error(`Unexpected command: ${command}`)
        })
        const controller = createDeviceSyncController({ facade })

        const internalFailure = await facade.start({ read: true, bidirectional: false }).catch((error) => error)
        expect(internalFailure).toBeInstanceOf(AggregateError)
        expect((internalFailure as AggregateError).errors).toEqual([primary, cleanup])

        await expect(controller.start({ read: true, bidirectional: false }))
            .rejects.toMatchObject({ code: 'operation-failed' })
        expect(controller.snapshot().error).toBe('operation-failed')
        expect(JSON.stringify(controller.snapshot())).not.toContain('private')
    })

    it('uses the exact identity returned by explicit native stop', async () => {
        const { facade, invoke, bridge } = fixture()

        await facade.stop()

        expect(invoke).toHaveBeenCalledWith('device_sync_stop')
        expect(bridge.stopSource).toHaveBeenCalledOnce()
        expect(bridge.stopSource).toHaveBeenCalledWith(
            'device-sync-source', foreground.operationId, foreground.generation,
        )
    })

    it('retries only the retained exact identity when the bridge initially rejects explicit stop', async () => {
        let stopAttempts = 0
        const { facade, invoke, bridge } = fixture({
            stopSource: () => {
                stopAttempts += 1
                return stopAttempts > 1
            },
        })

        await expect(facade.stop()).rejects.toMatchObject({ code: 'cleanup-failed' })
        await expect(facade.stop()).resolves.toBeUndefined()

        expect(invoke.mock.calls.filter(([command]) => command === 'device_sync_stop')).toHaveLength(1)
        expect(bridge.stopSource).toHaveBeenCalledTimes(2)
        expect(bridge.stopSource).toHaveBeenNthCalledWith(
            2, 'device-sync-source', foreground.operationId, foreground.generation,
        )
    })
})
