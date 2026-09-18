import { describe, expect, it, vi } from 'vitest'

import {
    continueCommittedWorkingSetRefresh,
    registerCommittedWorkingSetContinuation,
    retryCommittedWorkingSetRefreshWithContinuation,
} from './committedWorkingSetContinuation'

describe('committed working-set continuation', () => {
    it('waits for the committed revision and runs its continuation once', async () => {
        const runtime = {}
        const continuation = vi.fn(async () => {})
        registerCommittedWorkingSetContinuation(9, runtime, 2, continuation)

        await continueCommittedWorkingSetRefresh(8, runtime, 2)
        await Promise.all([
            continueCommittedWorkingSetRefresh(9, runtime, 2),
            continueCommittedWorkingSetRefresh(9, runtime, 2),
        ])
        await continueCommittedWorkingSetRefresh(10, runtime, 2)

        expect(continuation).toHaveBeenCalledOnce()
    })

    it('does not repeat a continuation that fails', async () => {
        const runtime = {}
        const continuation = vi.fn(async () => {
            throw new Error('plugin reload failed')
        })
        registerCommittedWorkingSetContinuation(11, runtime, 3, continuation)

        await expect(continueCommittedWorkingSetRefresh(11, runtime, 3)).rejects.toThrow(
            'plugin reload failed',
        )
        await continueCommittedWorkingSetRefresh(11, runtime, 3)

        expect(continuation).toHaveBeenCalledOnce()
    })

    it('discards a continuation from a replaced storage authority', async () => {
        const runtime = {}
        const stale = vi.fn(async () => {})
        registerCommittedWorkingSetContinuation(12, runtime, 4, stale)

        await continueCommittedWorkingSetRefresh(12, runtime, 5)

        expect(stale).not.toHaveBeenCalled()
    })

    it('discards a continuation from a disposed runtime', async () => {
        const stale = vi.fn(async () => {})
        registerCommittedWorkingSetContinuation(12, {}, 4, stale)

        await continueCommittedWorkingSetRefresh(12, {}, 4)

        expect(stale).not.toHaveBeenCalled()
    })

    it('runs the scoped continuation through the read-only retry entry', async () => {
        const continuation = vi.fn(async () => {})
        let authorityEpoch = 6
        const runtime = {
            retryCommittedWorkingSetRefresh: vi.fn(async () => {
                authorityEpoch++
                return {
                    kind: 'committed' as const,
                    revision: 14,
                    projection: 'applied' as const,
                }
            }),
            getStorageAuthorityEpoch: () => authorityEpoch,
        }
        registerCommittedWorkingSetContinuation(14, runtime, 6, continuation)

        await expect(
            retryCommittedWorkingSetRefreshWithContinuation(runtime),
        ).resolves.toMatchObject({ revision: 14, projection: 'applied' })

        expect(runtime.retryCommittedWorkingSetRefresh).toHaveBeenCalledOnce()
        expect(continuation).toHaveBeenCalledOnce()
    })

    it('prepares a committed retry once before refreshing and retains it when preparation fails', async () => {
        const events: string[] = []
        let preparationFails = true
        const beforeRefresh = vi.fn(async () => {
            events.push('before-refresh')
            if (preparationFails) throw new Error('native acknowledgement failed')
        })
        let refreshAttempts = 0
        const runtime = {
            retryCommittedWorkingSetRefresh: vi.fn(async () => {
                events.push('refresh')
                refreshAttempts++
                return {
                    kind: 'committed' as const,
                    revision: 17,
                    projection: refreshAttempts === 1
                        ? 'refresh-required' as const
                        : 'applied' as const,
                }
            }),
            getStorageAuthorityEpoch: () => 9,
        }
        const afterRefresh = vi.fn(async () => {
            events.push('after-refresh')
        })
        registerCommittedWorkingSetContinuation(
            17,
            runtime,
            9,
            afterRefresh,
            beforeRefresh,
        )

        await expect(
            retryCommittedWorkingSetRefreshWithContinuation(runtime),
        ).rejects.toThrow('native acknowledgement failed')
        expect(runtime.retryCommittedWorkingSetRefresh).not.toHaveBeenCalled()

        preparationFails = false
        await retryCommittedWorkingSetRefreshWithContinuation(runtime)
        expect(afterRefresh).not.toHaveBeenCalled()
        await retryCommittedWorkingSetRefreshWithContinuation(runtime)

        expect(events).toEqual([
            'before-refresh',
            'before-refresh',
            'refresh',
            'refresh',
            'after-refresh',
        ])
        expect(beforeRefresh).toHaveBeenCalledTimes(2)
        expect(runtime.retryCommittedWorkingSetRefresh).toHaveBeenCalledTimes(2)
        expect(afterRefresh).toHaveBeenCalledOnce()
    })

    it('keeps a successful read-only retry successful when its continuation fails', async () => {
        const report = vi.fn()
        let authorityEpoch = 7
        const runtime = {
            retryCommittedWorkingSetRefresh: vi.fn(async () => {
                authorityEpoch++
                return {
                    kind: 'committed' as const,
                    revision: 15,
                    projection: 'applied' as const,
                }
            }),
            getStorageAuthorityEpoch: () => authorityEpoch,
        }
        registerCommittedWorkingSetContinuation(15, runtime, 7, async () => {
            throw new Error('plugin reload failed')
        })

        await expect(
            retryCommittedWorkingSetRefreshWithContinuation(runtime, report),
        ).resolves.toMatchObject({ revision: 15, projection: 'applied' })
        expect(report).toHaveBeenCalledOnce()
    })

    it('rebases ownership when a read-only retry still needs another refresh', async () => {
        const continuation = vi.fn(async () => {})
        let authorityEpoch = 8
        let attempts = 0
        const runtime = {
            retryCommittedWorkingSetRefresh: vi.fn(async () => {
                authorityEpoch++
                attempts++
                return {
                    kind: 'committed' as const,
                    revision: 16,
                    projection: attempts === 1
                        ? 'refresh-required' as const
                        : 'applied' as const,
                }
            }),
            getStorageAuthorityEpoch: () => authorityEpoch,
        }
        registerCommittedWorkingSetContinuation(16, runtime, 8, continuation)

        await retryCommittedWorkingSetRefreshWithContinuation(runtime)
        expect(continuation).not.toHaveBeenCalled()
        await retryCommittedWorkingSetRefreshWithContinuation(runtime)

        expect(continuation).toHaveBeenCalledOnce()
    })
})
