import { beforeEach, describe, expect, it, vi } from 'vitest'

const bridge = vi.hoisted(() => ({ supported: true, getState: vi.fn() }))
const controller = vi.hoisted(() => ({ snapshot: vi.fn() }))

vi.mock('./sync/external/bridge', () => ({
    getExternalStorageBridge: () => bridge,
}))
vi.mock('./sync/serverSyncProduction', () => ({
    getServerSyncController: () => controller,
}))

import { combineLocalDataRemoteStates, readLocalDataRemoteState } from './localDataRemotes'

beforeEach(() => {
    bridge.supported = true
    bridge.getState.mockReset()
    controller.snapshot.mockReset()
    controller.snapshot.mockReturnValue({ status: { configured: false } })
    bridge.getState.mockResolvedValue({ connections: [] })
})

describe('combineLocalDataRemoteStates', () => {
    it('takes one connected remote over anything else', () => {
        expect(combineLocalDataRemoteStates(['unknown', 'connected'])).toBe('connected')
    })

    it('stays unknown while any answer is missing', () => {
        expect(combineLocalDataRemoteStates(['none', 'unknown'])).toBe('unknown')
    })

    it('answers none only when every remote answered none', () => {
        expect(combineLocalDataRemoteStates(['none', 'none'])).toBe('none')
    })
})

describe('readLocalDataRemoteState', () => {
    it('reports none when neither the sync server nor external storage is set up', async () => {
        await expect(readLocalDataRemoteState()).resolves.toBe('none')
    })

    it('reports connected for a bound sync server', async () => {
        controller.snapshot.mockReturnValue({ status: { configured: true } })
        await expect(readLocalDataRemoteState()).resolves.toBe('connected')
    })

    it('reports connected for an external storage connection', async () => {
        bridge.getState.mockResolvedValue({ connections: [{ id: 'connection-1' }] })
        await expect(readLocalDataRemoteState()).resolves.toBe('connected')
    })

    it('stays unknown until the server status has been read', async () => {
        controller.snapshot.mockReturnValue({})
        await expect(readLocalDataRemoteState()).resolves.toBe('unknown')
    })

    it('stays unknown when external storage cannot answer', async () => {
        bridge.getState.mockRejectedValue(new Error('external storage is busy'))
        await expect(readLocalDataRemoteState()).resolves.toBe('unknown')
    })
})
