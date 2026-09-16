import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const invoke = vi.hoisted(() => vi.fn())
const invalidateOwner = vi.hoisted(() => vi.fn())

vi.mock('@tauri-apps/api/core', () => ({ invoke }))
vi.mock('../platform', () => ({ isTauri: true }))
vi.mock('./plugins.svelte', () => ({
    pluginStorageStore: { invalidateOwner },
}))
const runStorageOnlyMutation = vi.hoisted(() => vi.fn())
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => ({ runStorageOnlyMutation }),
}))

import { PLUGIN_CLAIM_SESSION_LIMIT_MS, beginPluginClaimSession } from './pluginClaimSession'

const plugin = { name: 'provider-manager', script: '//@name provider-manager' }

function answers(overrides: Record<string, unknown> = {}) {
    invoke.mockImplementation(async (command: string) => {
        if (command in overrides) return overrides[command]
        if (command === 'pds_begin_plugin_claim_session') return 'session-one'
        if (command === 'pds_claim_plugin_storage_value') {
            return { value: { apiKey: 'imported' }, revision: 8 }
        }
        return undefined
    })
}

describe('plugin claim session', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        runStorageOnlyMutation.mockImplementation(
            (operation: (revision: number) => Promise<number>) => operation(7),
        )
        vi.useFakeTimers()
    })

    afterEach(() => {
        vi.useRealTimers()
    })

    it('opens nothing when no import is waiting', async () => {
        answers({ pds_begin_plugin_claim_session: null })
        await expect(beginPluginClaimSession(plugin)).resolves.toBeNull()
    })

    it('hands over the value and refreshes what the plugin can read', async () => {
        answers()
        const session = await beginPluginClaimSession(plugin)
        expect(session).not.toBeNull()

        await expect(session!.claim('pm_store')).resolves.toEqual({ apiKey: 'imported' })
        expect(invoke).toHaveBeenCalledWith('pds_claim_plugin_storage_value', {
            sessionId: 'session-one',
            owner: 'provider-manager',
            codeHash: expect.stringMatching(/^[0-9a-f]{64}$/),
            runtimeInstance: expect.any(String),
            key: 'pm_store',
            expectedRevision: 7,
        })
        expect(invalidateOwner).toHaveBeenCalledWith('provider-manager')
    })

    /** Invariant 24. */
    it('answers nothing once the window has closed and closes only once', async () => {
        answers()
        const session = await beginPluginClaimSession(plugin)
        await session!.close()
        await session!.close()

        await expect(session!.claim('pm_store')).resolves.toBeNull()
        const closes = invoke.mock.calls.filter(
            ([command]) => command === 'pds_close_plugin_claim_session',
        )
        expect(closes).toHaveLength(1)
        expect(
            invoke.mock.calls.some(([command]) => command === 'pds_claim_plugin_storage_value'),
        ).toBe(false)
    })

    it('answers a miss when the store refuses to move the value', async () => {
        answers()
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        runStorageOnlyMutation.mockImplementation(async () => {
            throw new Error('store closed')
        })
        const session = await beginPluginClaimSession(plugin)

        await expect(session!.claim('pm_store')).resolves.toBeNull()
        await expect(session!.claim('pm_keys')).resolves.toBeNull()
        expect(warn).toHaveBeenCalledTimes(1)
        expect(invalidateOwner).not.toHaveBeenCalled()
        warn.mockRestore()
    })

    it('waits for the replacement fence the import still holds', async () => {
        answers()
        const fenced = new Error('fenced')
        fenced.name = 'PersistentMutationFencedError'
        const session = await beginPluginClaimSession(plugin)
        runStorageOnlyMutation
            .mockImplementationOnce(async () => {
                throw fenced
            })
            .mockImplementation((operation: (revision: number) => Promise<number>) =>
                operation(7),
            )

        const claimed = session!.claim('pm_store')
        await vi.advanceTimersByTimeAsync(100)
        await expect(claimed).resolves.toEqual({ apiKey: 'imported' })
        expect(runStorageOnlyMutation).toHaveBeenCalledTimes(2)
    })

    it('stops waiting for a fence that never clears', async () => {
        answers()
        const fenced = new Error('fenced')
        fenced.name = 'PersistentMutationFencedError'
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        const session = await beginPluginClaimSession(plugin)
        runStorageOnlyMutation.mockImplementation(async () => {
            throw fenced
        })

        const claimed = session!.claim('pm_store')
        await vi.advanceTimersByTimeAsync(6_000)
        await expect(claimed).resolves.toBeNull()
        warn.mockRestore()
    })

    it('closes the window on its own bound when a plugin never finishes starting', async () => {
        answers()
        const session = await beginPluginClaimSession(plugin)
        await vi.advanceTimersByTimeAsync(PLUGIN_CLAIM_SESSION_LIMIT_MS)

        expect(
            invoke.mock.calls.some(([command]) => command === 'pds_close_plugin_claim_session'),
        ).toBe(true)
        await expect(session!.claim('pm_store')).resolves.toBeNull()
    })
})
