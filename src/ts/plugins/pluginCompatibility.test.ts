import { describe, expect, it, vi } from 'vitest'
import {
    createPluginLoadOrchestrator,
    createPluginLoadReentrancyGuard,
    runPluginUnloadCallbacks,
} from './pluginCompatibility'

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (error: unknown) => void
    const promise = new Promise<T>((res, rej) => {
        resolve = res
        reject = rej
    })
    return { promise, resolve, reject }
}

describe('plugin load orchestration', () => {
    it('serializes a late registry reset before a newer plugin runtime load', async () => {
        const resetStarted = deferred<void>()
        const finishReset = deferred<void>()
        const loadedV3: string[] = []
        let resets = 0
        let activeStages = 0
        let overlapped = false
        const load = createPluginLoadOrchestrator<string>({
            resetRegistry: async () => {
                activeStages++
                if (activeStages > 1) overlapped = true
                try {
                    resets++
                    if (resets === 1) {
                        resetStarted.resolve(undefined)
                        await finishReset.promise
                    }
                } finally {
                    activeStages--
                }
            },
            loadV3: async (plugins) => {
                activeStages++
                if (activeStages > 1) overlapped = true
                loadedV3.splice(0, loadedV3.length, ...plugins)
                activeStages--
            },
        })

        const disabling = load(['old-v3'])
        await resetStarted.promise
        let enablingSettled = false
        const enabling = load(['new-v3']).then(() => {
            enablingSettled = true
        })
        await Promise.resolve()
        expect(enablingSettled).toBe(false)
        finishReset.resolve(undefined)
        await disabling
        await enabling

        expect(overlapped).toBe(false)
        expect(resets).toBe(2)
        expect(loadedV3).toEqual(['new-v3'])
    })

    it('serializes an in-flight v3 load before the latest plugin runtime', async () => {
        const oldV3Started = deferred<void>()
        const finishOldV3 = deferred<void>()
        const loadedV3: string[] = []
        let activeStages = 0
        let overlapped = false
        const load = createPluginLoadOrchestrator<string>({
            resetRegistry: async () => {
                activeStages++
                if (activeStages > 1) overlapped = true
                activeStages--
            },
            loadV3: async (plugins) => {
                activeStages++
                if (activeStages > 1) overlapped = true
                try {
                    if (plugins.includes('old-v3')) {
                        oldV3Started.resolve(undefined)
                        await finishOldV3.promise
                    }
                    loadedV3.splice(0, loadedV3.length, ...plugins)
                } finally {
                    activeStages--
                }
            },
        })

        const oldLoad = load(['old-v3'])
        await oldV3Started.promise
        let latestSettled = false
        const latestLoad = load(['latest-v3']).then(() => {
            latestSettled = true
        })

        await Promise.resolve()
        expect(latestSettled).toBe(false)
        finishOldV3.resolve(undefined)
        await oldLoad
        await latestLoad

        expect(overlapped).toBe(false)
        expect(loadedV3).toEqual(['latest-v3'])
    })

    it('continues queued plugin loads after an earlier operation rejects', async () => {
        const failingV3Started = deferred<void>()
        const rejectFailingV3 = deferred<void>()
        const loadedV3: string[] = []
        const error = new Error('v3 unload failed')
        let activeStages = 0
        let overlapped = false
        const load = createPluginLoadOrchestrator<string>({
            resetRegistry: async () => undefined,
            loadV3: async (plugins) => {
                activeStages++
                if (activeStages > 1) overlapped = true
                try {
                    if (plugins.includes('failing-v3')) {
                        failingV3Started.resolve(undefined)
                        await rejectFailingV3.promise
                        throw error
                    }
                    loadedV3.splice(0, loadedV3.length, ...plugins)
                } finally {
                    activeStages--
                }
            },
        })

        const failingLoad = load(['failing-v3'])
        await failingV3Started.promise
        let recoverySettled = false
        const recoveryLoad = load(['recovered-v3']).then(() => {
            recoverySettled = true
        })

        await Promise.resolve()
        expect(recoverySettled).toBe(false)
        rejectFailingV3.resolve(undefined)
        await expect(failingLoad).rejects.toBe(error)
        await recoveryLoad

        expect(overlapped).toBe(false)
        expect(recoverySettled).toBe(true)
        expect(loadedV3).toEqual(['recovered-v3'])
    })

    it('skips the v3 load of a superseded generation', async () => {
        const resetStarted = deferred<void>()
        const finishReset = deferred<void>()
        const loadedV3: string[][] = []
        let resets = 0
        const load = createPluginLoadOrchestrator<string>({
            resetRegistry: async () => {
                resets++
                if (resets === 1) {
                    resetStarted.resolve(undefined)
                    await finishReset.promise
                }
            },
            loadV3: async (plugins) => {
                loadedV3.push([...plugins])
            },
        })

        const superseded = load(['stale-v3'])
        await resetStarted.promise
        const latest = load(['latest-v3'])
        finishReset.resolve(undefined)
        await superseded
        await latest

        expect(loadedV3).toEqual([['latest-v3']])
    })

    it('allows an awaited reentrant plugin load to queue without deadlocking its loader', async () => {
        const events: string[] = []
        const nestedComplete = deferred<void>()
        const guard = createPluginLoadReentrancyGuard((error) => {
            throw error
        })
        let load!: ReturnType<typeof createPluginLoadOrchestrator<string>>
        load = createPluginLoadOrchestrator<string>({
            resetRegistry: async () => undefined,
            loadV3: async (plugins) => {
                events.push(`start:${plugins.join(',')}`)
                await guard.runEvaluation(async () => {
                    if (plugins.includes('outer')) {
                        await guard.settle(load(['nested']))
                        events.push('outer-resumed')
                    }
                })
                events.push(`end:${plugins.join(',')}`)
                if (plugins.includes('nested')) nestedComplete.resolve(undefined)
            },
        })

        await load(['outer'])
        await nestedComplete.promise

        expect(events).toEqual([
            'start:outer',
            'outer-resumed',
            'end:outer',
            'start:nested',
            'end:nested',
        ])
    })

    it('allows an awaited reentrant load from an unload callback without deadlocking', async () => {
        const events: string[] = []
        const nestedComplete = deferred<void>()
        const guard = createPluginLoadReentrancyGuard((error) => {
            throw error
        })
        const unloads = new Set<() => Promise<void>>([
            async () => {
                await guard.settle(load(['nested']))
                events.push('unload-resumed')
            },
        ])
        let load!: ReturnType<typeof createPluginLoadOrchestrator<string>>
        load = createPluginLoadOrchestrator<string>({
            resetRegistry: async (isCurrent) => {
                events.push('reset')
                await runPluginUnloadCallbacks(unloads, isCurrent, guard)
            },
            loadV3: async (plugins) => {
                events.push(`load:${plugins.join(',')}`)
                if (plugins.includes('nested')) nestedComplete.resolve(undefined)
            },
        })

        await load([])
        await nestedComplete.promise

        expect(events).toEqual([
            'reset',
            'unload-resumed',
            'reset',
            'load:nested',
        ])
    })
})

describe('plugin unload callbacks', () => {
    it('drains every callback exactly once and reports the current generation', async () => {
        const guard = createPluginLoadReentrancyGuard((error) => {
            throw error
        })
        const calls: string[] = []
        const callbacks = new Set<() => void | Promise<void>>([
            () => { calls.push('first') },
            async () => { calls.push('second') },
        ])

        await expect(
            runPluginUnloadCallbacks(callbacks, () => true, guard),
        ).resolves.toBe(true)
        expect(calls).toEqual(['first', 'second'])
        expect(callbacks.size).toBe(0)
    })

    it('stops draining once the load generation is superseded', async () => {
        const guard = createPluginLoadReentrancyGuard((error) => {
            throw error
        })
        const later = vi.fn()
        let current = true
        const callbacks = new Set<() => void | Promise<void>>([
            () => { current = false },
            later,
        ])

        await expect(
            runPluginUnloadCallbacks(callbacks, () => current, guard),
        ).resolves.toBe(false)
        expect(later).not.toHaveBeenCalled()
        expect(callbacks.size).toBe(1)
    })
})

describe('plugin load reentrancy guard', () => {
    it('detaches a nested operation and reports its background failure', async () => {
        const failures: unknown[] = []
        const guard = createPluginLoadReentrancyGuard((error) => failures.push(error))
        const error = new Error('background load failed')
        let settled: Promise<void> | undefined

        await guard.runEvaluation(async () => {
            settled = guard.settle(Promise.reject(error))
            await settled
        })
        await Promise.resolve()

        expect(settled).toBeDefined()
        expect(failures).toEqual([error])
    })

    it('returns the operation itself outside an evaluation', async () => {
        const guard = createPluginLoadReentrancyGuard((error) => {
            throw error
        })
        const operation = Promise.resolve()

        expect(guard.settle(operation)).toBe(operation)
    })
})
