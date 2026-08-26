import { afterEach, describe, expect, it, vi } from 'vitest'
import {
    createLeadingEdgeScheduler,
    createStreamingDisplayController,
    type ScheduledSnapshot,
} from './streamingDisplayScheduler'

function deferred() {
    let resolve!: () => void
    let reject!: (error: unknown) => void
    const promise = new Promise<void>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, resolve, reject }
}

async function settle() {
    for (let index = 0; index < 6; index += 1) await Promise.resolve()
}

describe('leading-edge streaming display scheduler', () => {
    afterEach(() => {
        vi.useRealTimers()
    })

    it('starts the first provider snapshot immediately without creating a timer', async () => {
        vi.useFakeTimers()
        const started: ScheduledSnapshot<string>[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async (snapshot) => {
                started.push(snapshot)
            },
        })

        scheduler.submit('first')

        expect(started).toEqual([{ sequence: 1, value: 'first' }])
        expect(vi.getTimerCount()).toBe(0)
        await scheduler.finish()
    })

    it('keeps only the latest cumulative snapshot within the 125 ms cadence', async () => {
        vi.useFakeTimers()
        const started: string[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async ({ value }) => {
                started.push(value)
            },
        })

        scheduler.submit('a')
        await settle()
        scheduler.submit('ab')
        scheduler.submit('abc')
        scheduler.submit('abcd')
        expect(scheduler.inspect()).toMatchObject({ activeCount: 0, pendingCount: 1 })

        await vi.advanceTimersByTimeAsync(124)
        expect(started).toEqual(['a'])
        await vi.advanceTimersByTimeAsync(1)
        expect(started).toEqual(['a', 'abcd'])
        await scheduler.finish()
    })

    it('never has more than one active pass and one latest pending snapshot', async () => {
        vi.useFakeTimers()
        const first = deferred()
        let active = 0
        let maxActive = 0
        const scheduler = createLeadingEdgeScheduler({
            process: async ({ sequence }) => {
                active += 1
                maxActive = Math.max(maxActive, active)
                if (sequence === 1) await first.promise
                active -= 1
            },
        })

        scheduler.submit('a')
        scheduler.submit('ab')
        scheduler.submit('abc')

        expect(scheduler.inspect()).toMatchObject({ activeCount: 1, pendingCount: 1 })
        expect(maxActive).toBe(1)
        first.resolve()
        await vi.runAllTimersAsync()
        await scheduler.finish()
        expect(maxActive).toBe(1)
    })

    it('starts the latest pending snapshot at completion when a slow pass exceeds the cadence', async () => {
        vi.useFakeTimers()
        vi.setSystemTime(0)
        const first = deferred()
        const starts: Array<[string, number]> = []
        const scheduler = createLeadingEdgeScheduler({
            process: async ({ sequence, value }) => {
                starts.push([value, Date.now()])
                if (sequence === 1) await first.promise
            },
        })

        scheduler.submit('a')
        scheduler.submit('ab')
        await vi.advanceTimersByTimeAsync(200)
        first.resolve()
        await settle()

        expect(starts).toEqual([['a', 0], ['ab', 200]])
        await scheduler.finish()
    })

    it('waits only until the next start-time boundary after a fast pass', async () => {
        vi.useFakeTimers()
        vi.setSystemTime(0)
        const starts: number[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async () => {
                starts.push(Date.now())
            },
        })

        scheduler.submit('a')
        await settle()
        await vi.advanceTimersByTimeAsync(40)
        scheduler.submit('ab')
        await vi.advanceTimersByTimeAsync(84)
        expect(starts).toEqual([0])
        await vi.advanceTimersByTimeAsync(1)
        expect(starts).toEqual([0, 125])
        await scheduler.finish()
    })

    it('flushes the latest uncommitted snapshot once at normal EOF', async () => {
        vi.useFakeTimers()
        const started: string[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async ({ value }) => {
                started.push(value)
            },
        })

        scheduler.submit('a')
        await settle()
        scheduler.submit('ab')
        scheduler.submit('abc')
        await scheduler.finish()

        expect(started).toEqual(['a', 'abc'])
        expect(scheduler.inspect()).toMatchObject({ status: 'closed', activeCount: 0, pendingCount: 0, timerCount: 0 })
    })

    it('does not duplicate the active or completed snapshot at normal EOF', async () => {
        vi.useFakeTimers()
        const first = deferred()
        const sequences: number[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async ({ sequence }) => {
                sequences.push(sequence)
                await first.promise
            },
        })

        scheduler.submit('same')
        const finishing = scheduler.finish()
        first.resolve()
        await finishing

        expect(sequences).toEqual([1])
    })

    it('drops pending work when aborted before the timer fires', async () => {
        vi.useFakeTimers()
        const started: string[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async ({ value }) => {
                started.push(value)
            },
        })

        scheduler.submit('a')
        await settle()
        scheduler.submit('ab')
        await scheduler.abort()
        await vi.runAllTimersAsync()

        expect(started).toEqual(['a'])
        expect(scheduler.inspect()).toMatchObject({ status: 'aborted', activeCount: 0, pendingCount: 0, timerCount: 0 })
    })

    it('starts no later pass after abort during active work and retains the last successful commit', async () => {
        vi.useFakeTimers()
        const first = deferred()
        const commits: string[] = ['previous']
        const scheduler = createLeadingEdgeScheduler({
            process: async ({ value }, context) => {
                await first.promise
                if (context.canCommit()) commits.push(value)
            },
        })

        scheduler.submit('a')
        scheduler.submit('ab')
        const aborting = scheduler.abort()
        first.resolve()
        await aborting

        expect(commits).toEqual(['previous'])
        expect(scheduler.inspect()).toMatchObject({ status: 'aborted', activeCount: 0, pendingCount: 0 })
    })

    it('clears resources and propagates the original processor rejection', async () => {
        vi.useFakeTimers()
        const failure = new Error('processor failed')
        const reported: unknown[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async () => {
                throw failure
            },
            onError: (error) => {
                reported.push(error)
                throw new Error('cleanup failed')
            },
        })

        scheduler.submit('a')
        await settle()

        await expect(scheduler.finish()).rejects.toBe(failure)
        expect(reported).toEqual([failure])
        expect(scheduler.inspect()).toMatchObject({ status: 'failed', activeCount: 0, pendingCount: 0, timerCount: 0 })
    })

    it('treats a null processor rejection as a failure instead of an empty sentinel', async () => {
        vi.useFakeTimers()
        const reported: unknown[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async () => Promise.reject(null),
            onError: (error) => reported.push(error),
        })

        scheduler.submit('a')
        await settle()

        await expect(scheduler.finish()).rejects.toBeNull()
        expect(reported).toEqual([null])
        expect(scheduler.inspect()).toMatchObject({
            status: 'failed',
            activeCount: 0,
            pendingCount: 0,
            timerCount: 0,
        })
    })

    it('uses the mode pinned when the generation controller is created', async () => {
        vi.useFakeTimers()
        let configuredMode: 'off' | 'balanced' = 'balanced'
        const semantic: string[] = []
        const controller = createStreamingDisplayController({
            mode: configuredMode,
            processSemantic: async ({ value }) => {
                semantic.push(value)
            },
            processPreview: async () => {},
        })

        controller.submit('a')
        await settle()
        configuredMode = 'off'
        controller.submit('ab')
        controller.submit('abc')
        await controller.finish()

        expect(configuredMode).toBe('off')
        expect(controller.mode).toBe('balanced')
        expect(semantic).toEqual(['a', 'abc'])
    })

    it('uses sequence identity for empty and repeated-text provider snapshots', async () => {
        vi.useFakeTimers()
        const processed: ScheduledSnapshot<string>[] = []
        const scheduler = createLeadingEdgeScheduler({
            process: async (snapshot) => {
                processed.push(snapshot)
            },
        })

        scheduler.submit('')
        await settle()
        scheduler.submit('')
        await scheduler.finish()

        expect(processed).toEqual([
            { sequence: 1, value: '' },
            { sequence: 2, value: '' },
        ])
    })
})

describe('pinned streaming display modes', () => {
    afterEach(() => {
        vi.useRealTimers()
    })

    it.each([
        { mode: 'off' as const, expectedSemantic: 4, expectedPreview: 0 },
        { mode: 'balanced' as const, expectedSemantic: 2, expectedPreview: 0 },
        { mode: 'strong' as const, expectedSemantic: 1, expectedPreview: 2 },
    ])('characterizes regex, Lua, and plugin action counts in $mode mode', async ({ mode, expectedSemantic, expectedPreview }) => {
        vi.useFakeTimers()
        const actionCounts = { regex: 0, lua: 0, plugin: 0, preview: 0 }
        const controller = createStreamingDisplayController({
            mode,
            processSemantic: async () => {
                actionCounts.regex += 1
                actionCounts.lua += 1
                actionCounts.plugin += 1
            },
            processPreview: async () => {
                actionCounts.preview += 1
            },
        })

        await controller.submit('a')
        await controller.submit('ab')
        await controller.submit('abc')
        await controller.submit('abcd')
        await controller.finish()

        expect(actionCounts).toEqual({
            regex: expectedSemantic,
            lua: expectedSemantic,
            plugin: expectedSemantic,
            preview: expectedPreview,
        })
    })

    it.each(['off', 'balanced', 'strong'] as const)('starts no post-abort action or commit in %s mode', async (mode) => {
        vi.useFakeTimers()
        const active = deferred()
        const actions: string[] = []
        const commits: string[] = []
        const controller = createStreamingDisplayController({
            mode,
            processSemantic: async ({ value }, context) => {
                actions.push(`semantic:${value}`)
                await active.promise
                if (context.canCommit()) commits.push(value)
            },
            processPreview: async ({ value }, context) => {
                actions.push(`preview:${value}`)
                await active.promise
                if (context.canCommit()) commits.push(value)
            },
        })

        const first = controller.submit('a')
        const second = controller.submit('ab')
        const aborting = controller.abort()
        active.resolve()
        await Promise.all([first, second, aborting])

        expect(actions).toEqual([`${mode === 'strong' ? 'preview' : 'semantic'}:a`])
        expect(commits).toEqual([])
    })

    it('keeps the exact submit rejection authoritative when cleanup also throws', async () => {
        vi.useFakeTimers()
        const processorFailure = new Error('processor failed')
        const controller = createStreamingDisplayController({
            mode: 'off',
            processSemantic: async () => {
                throw processorFailure
            },
            processPreview: async () => {},
            onError: () => {
                throw new Error('cleanup failed')
            },
        })
        let outcome: unknown = 'pending'

        void controller.submit('a').then(
            () => { outcome = 'resolved' },
            (error) => { outcome = error },
        )
        await settle()

        expect(outcome).toBe(processorFailure)
        await expect(controller.finish()).rejects.toBe(processorFailure)
        expect(controller.inspect()).toMatchObject({
            status: 'failed',
            activeCount: 0,
            pendingCount: 0,
        })
    })
})
