import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const invoke = vi.hoisted(() => vi.fn())
const invalidateOwner = vi.hoisted(() => vi.fn())

vi.mock('@tauri-apps/api/core', () => ({ invoke }))
vi.mock('../platform', () => ({ isTauri: true }))
vi.mock('./plugins.svelte', () => ({
    pluginStorageStore: { invalidateOwner },
}))

import { PLUGIN_CLAIM_SESSION_LIMIT_MS, beginPluginClaimSession } from './pluginClaimSession'

const plugin = { name: 'provider-manager', script: '//@name provider-manager' }

function answers(overrides: Record<string, unknown> = {}) {
    invoke.mockImplementation(async (command: string) => {
        if (command in overrides) return overrides[command]
        if (command === 'pds_begin_plugin_claim_session') return 'session-one'
        if (command === 'pds_claim_plugin_storage_value') return { apiKey: 'imported' }
        return undefined
    })
}

describe('plugin claim session', () => {
    beforeEach(() => {
        vi.clearAllMocks()
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
