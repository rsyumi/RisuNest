// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import type { Writable } from 'svelte/store'
import type { CommittedApplyOutcome } from 'src/ts/storage/persistentDataRuntime'

const mocks = vi.hoisted(() => ({ retry: vi.fn(), retryExternal: vi.fn() }))
vi.mock('src/ts/storage/sync/external/applicationRecovery', async () => {
    const { writable } = await import('svelte/store')
    return {
        externalApplicationRecovery: writable(null),
        retryExternalApplication: mocks.retryExternal,
    }
})
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        persistentWorkingSetRefreshRevision: writable<number | null>(null),
        retryCommittedWorkingSetRefresh: mocks.retry,
    }
})
vi.mock('src/lang', async () => ({
    language: (await import('src/lang/en')).languageEnglish,
}))

import { externalApplicationRecovery } from 'src/ts/storage/sync/external/applicationRecovery'
import Recovery from './PersistentWorkingSetRecovery.svelte'
import { persistentWorkingSetRefreshRevision } from 'src/ts/storage/persistentDataRuntime.svelte'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'

const external = externalApplicationRecovery as Writable<{ jobId: string; confirmationPending: boolean } | null>
const revision = persistentWorkingSetRefreshRevision as Writable<number | null>
const copy = languageEnglish.risuNest.persistentData

describe('committed working-set recovery', () => {
    let mounted: ReturnType<typeof mount> | undefined
    let target: HTMLDivElement

    beforeEach(() => {
        mocks.retry.mockReset()
        mocks.retryExternal.mockReset()
        external.set(null)
        revision.set(null)
        target = document.createElement('div')
        document.body.append(target)
    })
    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
    })
    async function setup(pending: number | null): Promise<void> {
        revision.set(pending)
        mounted = mount(Recovery, { target })
        await tick()
        await tick()
    }

    it('does not open or start recovery when no committed revision needs refresh', async () => {
        await setup(null)
        expect(target.querySelector('[role="dialog"]')).toBeNull()
        expect(mocks.retry).not.toHaveBeenCalled()
    })

    it('explains the committed result and focuses the recovery dialog', async () => {
        await setup(8)
        expect(target.textContent).toContain(copy.refreshTitle)
        expect(target.textContent).toContain(copy.refreshHelp)
        expect(document.activeElement).toBe(target.querySelector('[role="dialog"]'))
        expect(mocks.retry).not.toHaveBeenCalled()
    })

    it('starts one read-only retry and closes only after the runtime clears its guard', async () => {
        let finish!: (outcome: CommittedApplyOutcome) => void
        mocks.retry.mockImplementation(() => new Promise<CommittedApplyOutcome>((resolve) => { finish = resolve }))
        await setup(8)
        const button = target.querySelector('button')!
        button.click()
        await tick()
        expect(button.disabled).toBe(true)
        button.click()
        expect(mocks.retry).toHaveBeenCalledOnce()
        expect(target.querySelector('[role="dialog"]')).not.toBeNull()
        revision.set(null)
        finish({ kind: 'committed', revision: 8, projection: 'applied' })
        await vi.waitFor(() => expect(target.querySelector('[role="dialog"]')).toBeNull())
        expect(mocks.retry).toHaveBeenCalledOnce()
    })

    it.each(['returned', 'thrown'] as const)('keeps the guard and permits another read after a %s refresh failure', async (failure) => {
        if (failure === 'returned') {
            mocks.retry.mockResolvedValue({ kind: 'committed', revision: 8, projection: 'refresh-required' })
        } else {
            mocks.retry.mockRejectedValue(new Error('read unavailable'))
        }
        await setup(8)
        target.querySelector('button')!.click()
        await vi.waitFor(() => expect(target.querySelector('[role="alert"]')?.textContent).toBe(copy.refreshFailed))
        expect(target.querySelector('[role="dialog"]')).not.toBeNull()
        expect(target.querySelector('button')!.disabled).toBe(false)
        expect(mocks.retry).toHaveBeenCalledOnce()
    })

    it('distinguishes an unconfirmed operation from a confirmed commit and retries its owner', async () => {
        external.set({ jobId: 'synthetic', confirmationPending: true })
        mocks.retryExternal.mockRejectedValueOnce(new Error('still unknown'))
        await setup(null)
        expect(target.textContent).toContain(copy.confirmApplicationTitle)
        expect(target.textContent).not.toContain(copy.refreshHelp)
        expect(target.querySelector('button')?.textContent?.trim()).toBe(copy.confirmApplication)
        target.querySelector('button')!.click()
        await vi.waitFor(() => expect(target.textContent).toContain(copy.confirmApplicationFailed))
        expect(mocks.retry).not.toHaveBeenCalled()
        expect(mocks.retryExternal).toHaveBeenCalledOnce()
    })

    it('keeps plugin-only recovery available after the runtime guard has cleared', async () => {
        external.set({ jobId: 'synthetic', confirmationPending: false })
        mocks.retryExternal.mockImplementation(async () => { external.set(null) })
        await setup(null)
        expect(target.textContent).toContain(copy.refreshTitle)
        target.querySelector('button')!.click()
        await vi.waitFor(() => expect(target.querySelector('[role="dialog"]')).toBeNull())
        expect(mocks.retry).not.toHaveBeenCalled()
        expect(mocks.retryExternal).toHaveBeenCalledOnce()
    })

    it('provides Korean and English text for every recovery message', () => {
        for (const key of Object.keys(copy) as Array<keyof typeof copy>) {
            expect(languageKorean.risuNest.persistentData[key], key).toBeTruthy()
        }
    })
})
