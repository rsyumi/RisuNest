import { afterEach, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import type { ExternalConnectionSummary } from './external/types'
import { syncStatus, type SyncStatusSource } from './syncStatus'
import { installProductionSyncStatus } from './syncStatusProduction'

const state = vi.hoisted(() => ({ failure: undefined as unknown }))
vi.mock('./bindingNative', () => ({ createNativeSyncBindingBridge: () => ({ state: async () => ({ target: { kind: 'external', connectionId: 'webdav-1' } }) }) }))
vi.mock('./serverSyncProduction', () => ({ getServerSyncController: vi.fn(), retryServerSync: vi.fn() }))
vi.mock('./external/bridge', () => ({
    getExternalStorageBridge: () => ({
        getState: async () => ({
            selection: { kind: 'external', connectionId: 'webdav-1', selectionEpoch: '0', paused: false },
            connections: ['webdav-1', 'drive-2'].map(id => ({ id, purpose: id === 'webdav-1' ? 'sync' : 'backup', status: 'ready' }) as Partial<ExternalConnectionSummary>),
            jobs: [],
        }),
    }),
}))
vi.mock('./external/lwwProduction', () => ({
    isExternalLwwRunning: () => true,
    requestExternalLwwNow: vi.fn(),
    subscribeExternalLwwCatchUp(listener) { listener(new Set()); return () => {} },
    subscribeExternalLwwFailures(listener) { listener(new Map([['webdav-1', state.failure]])); return () => {} },
} satisfies Partial<typeof import('./external/lwwProduction')>))

let uninstall = () => {}
afterEach(() => uninstall())

it.each([
    // An asset download reads every storage connection that holds a body and names the one that stopped it.
    ['another storage an asset download stopped on', { code: 'external-key-locked', status: 502, retryable: false, connectionId: 'drive-2' }, 'drive-2'],
    ['no storage for a stop on the bound one', { kind: 'repositoryKeyUnavailable' }, undefined],
    ['no storage for one that is gone', { code: 'external-reauth-required', status: 502, retryable: false, connectionId: 'drive-3' }, undefined],
])('names %s', async (_, failure, named) => {
    state.failure = failure
    uninstall = installProductionSyncStatus()
    await vi.waitFor(() => expect(get(syncStatus)).toMatchObject({ target: 'external', failure }))
    expect((get(syncStatus) as Extract<SyncStatusSource, { target: 'external' }>).remedyConnection?.id).toBe(named)
})
