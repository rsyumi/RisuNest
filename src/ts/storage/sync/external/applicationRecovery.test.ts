import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import type { CommittedApplyOutcome } from '../../persistentDataRuntime'

async function fixture() {
    const recovery = await import('./applicationRecovery')
    const outcome: CommittedApplyOutcome = { kind: 'committed', revision: 8, projection: 'applied' }
    const application = {
        jobId: 'synthetic-operation',
        fence: {
            revision: 7,
            release: vi.fn(),
            refreshCommittedWorkingSet: vi.fn(async () => outcome),
        },
        confirm: vi.fn(async () => ({ kind: 'committed' as const, revision: 8 })),
        refreshReleased: vi.fn(async () => outcome),
        afterRefresh: vi.fn(async () => {}),
        settled: vi.fn(),
    }
    return { recovery, application }
}

describe('external application outcome ownership', () => {
    beforeEach(() => vi.resetModules())

    it('holds the original fence across an unknown reply and coalesces confirmation retries', async () => {
        const { recovery, application } = await fixture()
        application.confirm.mockRejectedValueOnce(new Error('synthetic lost reply'))
        await expect(recovery.runExternalApplication(application)).rejects.toThrow('lost reply')
        expect(application.fence.release).not.toHaveBeenCalled()
        expect(application.settled).not.toHaveBeenCalled()
        expect(get(recovery.externalApplicationRecovery)).toEqual({
            jobId: application.jobId, confirmationPending: true,
        })
        expect(() => recovery.runExternalApplication(application)).toThrow('already needs confirmation')
        await Promise.all([recovery.retryExternalApplication(), recovery.retryExternalApplication()])
        expect(application.confirm).toHaveBeenCalledTimes(2)
        expect(application.fence.refreshCommittedWorkingSet).toHaveBeenCalledOnce()
        expect(application.fence.release).toHaveBeenCalledOnce()
        expect(application.afterRefresh).toHaveBeenCalledOnce()
        expect(application.settled).toHaveBeenCalledOnce()
        expect(recovery.hasPendingExternalApplication()).toBe(false)
    })

    it('releases only a confirmed non-application without attempting a screen refresh', async () => {
        const { recovery, application } = await fixture()
        await expect(recovery.runExternalApplication({
            ...application,
            confirm: async () => ({ kind: 'not-applied', error: new Error('rejected before mutation') }),
        })).rejects.toThrow('rejected before mutation')
        expect(application.fence.release).toHaveBeenCalledOnce()
        expect(application.fence.refreshCommittedWorkingSet).not.toHaveBeenCalled()
        expect(application.settled).toHaveBeenCalledOnce()
        expect(get(recovery.externalApplicationRecovery)).toBeNull()
    })

    it('rejects an invalid receipt without unlocking editing', async () => {
        const { recovery, application } = await fixture()
        application.confirm.mockResolvedValueOnce({ kind: 'committed', revision: Number.NaN })
        await expect(recovery.runExternalApplication(application)).rejects.toThrow('invalid revision')
        expect(application.fence.release).not.toHaveBeenCalled()
        await recovery.retryExternalApplication()
        expect(application.confirm).toHaveBeenCalledTimes(2)
    })

    it('retains the fence when refresh throws before installing a read-only guard', async () => {
        const { recovery, application } = await fixture()
        application.fence.refreshCommittedWorkingSet.mockRejectedValueOnce(new Error('refresh interrupted'))
        await expect(recovery.runExternalApplication(application)).rejects.toThrow('refresh interrupted')
        expect(application.fence.release).not.toHaveBeenCalled()
        await recovery.retryExternalApplication()
        expect(application.confirm).toHaveBeenCalledOnce()
        expect(application.fence.refreshCommittedWorkingSet).toHaveBeenCalledTimes(2)
    })

    it('retries a committed projection read-only and never starts the operation twice', async () => {
        const { recovery, application } = await fixture()
        application.fence.refreshCommittedWorkingSet.mockResolvedValueOnce({
            kind: 'committed', revision: 8, projection: 'refresh-required',
        })
        await expect(recovery.runExternalApplication(application)).rejects.toThrow('read-only')
        expect(application.fence.release).toHaveBeenCalledOnce()
        expect(application.afterRefresh).not.toHaveBeenCalled()
        expect(get(recovery.externalApplicationRecovery)?.confirmationPending).toBe(false)
        await recovery.retryExternalApplication()
        expect(application.refreshReleased).toHaveBeenCalledWith(8)
        expect(application.confirm).toHaveBeenCalledOnce()
        expect(application.fence.refreshCommittedWorkingSet).toHaveBeenCalledOnce()
        expect(application.afterRefresh).toHaveBeenCalledOnce()
    })

    it('retries only plugin and routing refresh after projection succeeded', async () => {
        const { recovery, application } = await fixture()
        application.afterRefresh.mockRejectedValueOnce(new Error('synthetic plugin failure'))
        await expect(recovery.runExternalApplication(application)).rejects.toThrow('plugin failure')
        await recovery.retryExternalApplication()
        expect(application.afterRefresh).toHaveBeenCalledTimes(2)
        expect(application.confirm).toHaveBeenCalledOnce()
        expect(application.fence.refreshCommittedWorkingSet).toHaveBeenCalledOnce()
        expect(application.refreshReleased).not.toHaveBeenCalled()
    })
})
