import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(async (_command: string, _args?: Record<string, unknown>): Promise<unknown> => undefined),
    flushPendingData: vi.fn(async (_reason: string) => {}),
    restartNativeApp: vi.fn(async () => {}),
    alertConfirm: vi.fn(async (_message: string) => true),
    alertError: vi.fn((_message: string | Error) => {}),
    alertNormal: vi.fn((_message: string) => {}),
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke, Channel: class { onmessage = (_value: unknown) => {} } }))
vi.mock('./persistentDataRuntime.svelte', () => ({ flushPendingData: mocks.flushPendingData }) satisfies Partial<typeof import('./persistentDataRuntime.svelte')>)
vi.mock('./nativePersistentMaintenance', () => ({ restartNativeApp: mocks.restartNativeApp }) satisfies Partial<typeof import('./nativePersistentMaintenance')>)
vi.mock('../alert', () => ({
    alertConfirm: mocks.alertConfirm, alertError: mocks.alertError, alertNormal: mocks.alertNormal,
}) satisfies Partial<typeof import('../alert')>)

import { language } from 'src/lang'
import { compactRequestedStoreAtStart, requestStoreCompaction, showStoreCompactionNotice } from './storeCompaction'
import { formatRisuNestStorageBytes } from './risuNestStorageDashboard'

const strings = language.risuNest.storage
const GiB = 1024 ** 3

beforeEach(() => {
    vi.clearAllMocks()
    vi.spyOn(console, 'error').mockImplementation(() => undefined)
})

/** Commands answered by name; `pds_store_compaction_request` grants the request unless given. */
function answer(replies: Record<string, (args?: Record<string, unknown>) => unknown>): string[] {
    const commands: string[] = []
    mocks.invoke.mockImplementation(async (command, args) => {
        commands.push(command)
        if (command in replies) return replies[command](args)
        if (command === 'pds_store_compaction_request') return { kind: 'requested' }
        throw new Error(`unexpected command ${command}`)
    })
    return commands
}

describe('store compaction at start', () => {
    it('shows the copied share below 100 percent and returns the result', async () => {
        const result = { kind: 'compacted', reclaimedBytes: 3 * GiB }
        answer({
            pds_store_compaction_at_start: (args) => {
                const progress = args?.progress as { onmessage(value: unknown): void }
                progress.onmessage({ doneBytes: 50, totalBytes: 200 })
                progress.onmessage({ doneBytes: 200, totalBytes: 200 })
                return result
            },
        })
        const shown: string[] = []
        expect(await compactRequestedStoreAtStart((text) => shown.push(text))).toEqual(result)
        expect(shown).toEqual(['25', '99'].map((percent) => language.risuNest.startup.compacting.replace('{0}', percent)))
    })

    it('opens the store as usual when the compaction command fails', async () => {
        answer({ pds_store_compaction_at_start: () => { throw new Error('synthetic') } })
        expect(await compactRequestedStoreAtStart(() => {})).toBeNull()
    })
})

describe('store compaction notice', () => {
    it.each([
        [{ kind: 'compacted', reclaimedBytes: 3 * GiB } as const, 'normal', strings.compacted.replace('{0}', formatRisuNestStorageBytes(3 * GiB))],
        [{ kind: 'no-space', shortfallBytes: 2 * GiB } as const, 'error', strings.compactStartNoSpace.replace('{0}', formatRisuNestStorageBytes(2 * GiB))],
        [{ kind: 'failed' } as const, 'error', strings.compactFailed],
    ])('reports the result of a compacting start instead of offering (%o)', async (result, kind, message) => {
        const commands = answer({})
        await showStoreCompactionNotice(result)
        expect(kind === 'normal' ? mocks.alertNormal : mocks.alertError).toHaveBeenCalledExactlyOnceWith(message)
        expect(commands).toEqual([])
        expect(mocks.alertConfirm).not.toHaveBeenCalled()
    })

    it('offers the reclaimable size and requests compaction when accepted', async () => {
        const commands = answer({ pds_store_compaction_offer: () => 5 * GiB })
        await showStoreCompactionNotice(null)
        expect(mocks.alertConfirm).toHaveBeenCalledExactlyOnceWith(strings.compactOffer.replace('{0}', formatRisuNestStorageBytes(5 * GiB)))
        expect(commands).toEqual(['pds_store_compaction_offer', 'pds_store_compaction_request'])
        expect(mocks.restartNativeApp).toHaveBeenCalledOnce()

        mocks.alertConfirm.mockResolvedValueOnce(false)
        const declined = answer({ pds_store_compaction_offer: () => 5 * GiB })
        await showStoreCompactionNotice(null)
        expect(declined).toEqual(['pds_store_compaction_offer'])
        const unoffered = answer({ pds_store_compaction_offer: () => null })
        await showStoreCompactionNotice(null)
        expect(unoffered).toEqual(['pds_store_compaction_offer'])
        expect(mocks.alertConfirm).toHaveBeenCalledTimes(2)
        expect(mocks.restartNativeApp).toHaveBeenCalledOnce()
    })
})

describe('store compaction request', () => {
    it('flushes pending data, records the request, then restarts', async () => {
        const order: string[] = []
        mocks.flushPendingData.mockImplementationOnce(async () => { order.push('flush') })
        answer({ pds_store_compaction_request: () => { order.push('request'); return { kind: 'requested' } } })
        mocks.restartNativeApp.mockImplementationOnce(async () => { order.push('restart') })
        await requestStoreCompaction()
        expect(order).toEqual(['flush', 'request', 'restart'])
    })

    it('names the shortfall and does not restart when the volume lacks room', async () => {
        answer({ pds_store_compaction_request: () => ({ kind: 'no-space', shortfallBytes: 2 * GiB }) })
        await requestStoreCompaction()
        expect(mocks.alertError).toHaveBeenCalledExactlyOnceWith(strings.compactNoSpace.replace('{0}', formatRisuNestStorageBytes(2 * GiB)))
        expect(mocks.restartNativeApp).not.toHaveBeenCalled()
    })

    it('records nothing when pending data cannot be saved, and says when the restart fails', async () => {
        const commands = answer({})
        mocks.flushPendingData.mockRejectedValueOnce(new Error('synthetic'))
        await expect(requestStoreCompaction()).rejects.toThrow('synthetic')
        expect(commands).toEqual([])
        expect(mocks.restartNativeApp).not.toHaveBeenCalled()

        mocks.restartNativeApp.mockRejectedValueOnce(new Error('synthetic restart'))
        await requestStoreCompaction()
        expect(commands).toEqual(['pds_store_compaction_request'])
        expect(mocks.alertError).toHaveBeenCalledExactlyOnceWith(strings.compactRestartFailed)
    })
})
