import { describe, expect, it, vi } from 'vitest'
import {
    assertPluginFullObjectCompatibility,
    createAwaitablePluginLoaderSource,
    createFullCompatibilityPersistence,
    createPluginCompatibilityController,
    createPluginLoadOrchestrator,
    createPluginLoadReentrancyGuard,
    getManualPluginInstallVersion,
    runPluginFullObjectReplacement,
    runPluginUnloadCallbacks,
    runAwaitablePluginLoader,
    selectPluginCompatibilityProfile,
    shouldProjectScalableWorkingSet,
    type PluginCompatibilityController,
    type PluginCompatibilityProfile,
} from './pluginCompatibility'
import { createPluginDatabaseAccess } from './pluginDatabaseAccess'
import type { Database } from '../storage/database.svelte'
import type { PersistentDataStore } from '../storage/persistentDataStore'

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (error: unknown) => void
    const promise = new Promise<T>((res, rej) => {
        resolve = res
        reject = rej
    })
    return { promise, resolve, reject }
}

describe('manual plugin installation compatibility', () => {
    it('allows API v2.1 and v3.0 while rejecting v2.0', () => {
        expect(getManualPluginInstallVersion('2.1')).toBe('2.1')
        expect(getManualPluginInstallVersion('3.0')).toBe('3.0')
        expect(getManualPluginInstallVersion('2.0')).toBeNull()
    })
})

describe('plugin compatibility profiles', () => {
    it('preserves errors and identifies the V2 plugin in evaluated code stacks', async () => {
        const source = createAwaitablePluginLoaderSource(
            "throw new TypeError('synthetic failure')",
            'fixture/with\nnewline',
        )
        let caught: Error | undefined
        try {
            await runAwaitablePluginLoader(source)
        } catch (error) {
            caught = error as Error
        }
        expect(caught).toBeInstanceOf(TypeError)
        expect(caught?.stack).toContain(
            'risu-plugin-v2/fixture%2Fwith%0Anewline.js',
        )
        expect(caught?.message).toBe('synthetic failure')
    })

    it('keeps direct maximum full-object replacements gated', () => {
        expect(() => assertPluginFullObjectCompatibility(
            'scalable-v3',
            'setCharacter',
        )).toThrow(/setCharacter.*maximum-compatibility.*queryCharacters/i)

        expect(() => assertPluginFullObjectCompatibility(
            'maximum-compatibility',
            'setCharacter',
        )).not.toThrow()
    })

    it('invalidates the active conversation after a full-object replacement', () => {
        const events: string[] = []

        const result = runPluginFullObjectReplacement(
            'maximum-compatibility',
            'setCharacter',
            true,
            () => {
                events.push('replace')
                return 'replaced'
            },
            () => events.push('invalidate'),
        )

        expect(result).toBe('replaced')
        expect(events).toEqual(['replace', 'invalidate'])
    })

    it('selects scalable mode unless an enabled API v2.1 plugin exists', () => {
        expect(selectPluginCompatibilityProfile([])).toBe('scalable-v3')
        expect(selectPluginCompatibilityProfile([{ version: '3.0', enabled: true }])).toBe(
            'scalable-v3',
        )
        expect(selectPluginCompatibilityProfile([{ version: '2.1', enabled: false }])).toBe(
            'scalable-v3',
        )
        expect(selectPluginCompatibilityProfile([{ version: 2, enabled: true }])).toBe(
            'scalable-v3',
        )
        expect(selectPluginCompatibilityProfile([{ version: '2.1', enabled: true }])).toBe(
            'maximum-compatibility',
        )
    })

    it('blocks eviction immediately when maximum compatibility is enabled', async () => {
        const persist = vi.fn(async () => undefined)
        const controller = createPluginCompatibilityController(persist)

        const transition = controller.transition('maximum-compatibility')

        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(false)
        await transition
        expect(controller.profile).toBe('maximum-compatibility')
        expect(persist).not.toHaveBeenCalled()
    })

    it('keeps internal replacements complete until a scalable publication is authorized', async () => {
        const maximum = deferred<void>()
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            enterMaximumCompatibility: () => maximum.promise,
        })

        const transition = controller.transition('maximum-compatibility')

        expect(controller.profile).toBe('scalable-v3')
        expect(shouldProjectScalableWorkingSet(controller)).toBe(false)
        expect(shouldProjectScalableWorkingSet(controller, true)).toBe(true)

        maximum.resolve(undefined)
        await transition
        expect(shouldProjectScalableWorkingSet(controller)).toBe(false)
    })

    it('restores the scalable profile and eviction state when maximum materialization fails', async () => {
        const error = new Error('maximum materialization failed')
        const setEvictionAllowed = vi.fn()
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            enterMaximumCompatibility: vi.fn(async () => {
                throw error
            }),
            setEvictionAllowed,
        })

        await expect(controller.transition('maximum-compatibility')).rejects.toBe(error)

        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(true)
        expect(setEvictionAllowed).toHaveBeenLastCalledWith(true)
    })

    it('returns and awaits the async v2.1 loader before completing', async () => {
        const gate = deferred<void>()
        const globalState = globalThis as typeof globalThis & {
            __pluginLoaderGate?: Promise<void>
            __pluginLoaderComplete?: boolean
        }
        globalState.__pluginLoaderGate = gate.promise
        globalState.__pluginLoaderComplete = false

        try {
            let settled = false
            const loading = runAwaitablePluginLoader(createAwaitablePluginLoaderSource(`
                await globalThis.__pluginLoaderGate
                globalThis.__pluginLoaderComplete = true
            `)).then(() => {
                settled = true
            })

            await Promise.resolve()
            expect(settled).toBe(false)
            expect(globalState.__pluginLoaderComplete).toBe(false)

            gate.resolve(undefined)
            await loading

            expect(globalState.__pluginLoaderComplete).toBe(true)
        } finally {
            delete globalState.__pluginLoaderGate
            delete globalState.__pluginLoaderComplete
        }
    })

    it('materializes maximum compatibility after blocking eviction and releases only after persistence', async () => {
        const events: string[] = []
        const enterMaximum = deferred<void>()
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: async () => {
                events.push('persist-complete-snapshot')
            },
            enterMaximumCompatibility: async () => {
                events.push('enter-maximum')
                await enterMaximum.promise
            },
            setEvictionAllowed: (allowed) => {
                events.push(`eviction:${allowed}`)
            },
            releaseAfterScalable: () => {
                events.push('release-scalable')
            },
        })

        const maximum = controller.transition('maximum-compatibility')
        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(false)
        expect(events).toEqual(['eviction:false', 'enter-maximum'])
        enterMaximum.resolve(undefined)
        await maximum
        expect(controller.profile).toBe('maximum-compatibility')

        await controller.transition('scalable-v3')

        expect(events).toEqual([
            'eviction:false',
            'enter-maximum',
            'persist-complete-snapshot',
            'release-scalable',
            'eviction:true',
        ])
    })

    it('keeps eviction blocked during generation and retries scalable release when generation ends', async () => {
        let generationActive = true
        const releaseOpportunity = deferred<void>()
        const persistBeforeEviction = vi.fn(async () => undefined)
        const releaseAfterScalable = vi.fn()
        const setEvictionAllowed = vi.fn()
        const cancelReleaseRetry = vi.fn()
        const scheduleReleaseRetry = vi.fn((retry: () => void) => {
            void releaseOpportunity.promise.then(retry)
            return cancelReleaseRetry
        })
        const controller = createPluginCompatibilityController({
            persistBeforeEviction,
            canReleaseWorkingSet: () => !generationActive,
            scheduleReleaseRetry,
            releaseAfterScalable,
            setEvictionAllowed,
        })
        await controller.transition('maximum-compatibility')

        await expect(controller.transition('scalable-v3')).resolves.toBe('scalable-v3')

        expect(controller.allowsEviction).toBe(false)
        expect(persistBeforeEviction).toHaveBeenCalledTimes(1)
        expect(setEvictionAllowed).not.toHaveBeenCalledWith(true)
        expect(releaseAfterScalable).not.toHaveBeenCalled()
        expect(scheduleReleaseRetry).toHaveBeenCalledOnce()

        await controller.transition('scalable-v3')
        expect(scheduleReleaseRetry).toHaveBeenCalledOnce()
        expect(persistBeforeEviction).toHaveBeenCalledOnce()

        generationActive = false
        releaseOpportunity.resolve(undefined)
        await vi.waitFor(() => expect(releaseAfterScalable).toHaveBeenCalledOnce())

        expect(controller.allowsEviction).toBe(true)
        expect(persistBeforeEviction).toHaveBeenCalledOnce()
        expect(setEvictionAllowed).toHaveBeenCalledWith(true)
        expect(releaseAfterScalable).toHaveBeenCalledTimes(1)
    })

    it('rechecks generation before scalable publication and retries after a release race', async () => {
        let generationActive = false
        const firstRelease = deferred<boolean>()
        const retries: Array<() => void> = []
        const setEvictionAllowed = vi.fn()
        const releaseAfterScalable = vi.fn()
            .mockImplementationOnce(() => firstRelease.promise)
            .mockResolvedValueOnce(true)
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            canReleaseWorkingSet: () => !generationActive,
            scheduleReleaseRetry: (retry) => {
                retries.push(retry)
                return vi.fn()
            },
            releaseAfterScalable,
            setEvictionAllowed,
        })
        await controller.transition('maximum-compatibility')

        const transition = controller.transition('scalable-v3')
        await vi.waitFor(() => expect(releaseAfterScalable).toHaveBeenCalledOnce())
        generationActive = true
        firstRelease.resolve(false)
        await transition

        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(false)
        expect(setEvictionAllowed).toHaveBeenLastCalledWith(false)
        expect(retries).toHaveLength(1)

        generationActive = false
        retries[0]()
        await vi.waitFor(() => expect(releaseAfterScalable).toHaveBeenCalledTimes(2))
        expect(controller.allowsEviction).toBe(true)
    })

    it('rearms a bounded scalable release retry after materialization fails', async () => {
        const error = new Error('navigation invalidated materialization')
        const retries: Array<() => void> = []
        const releaseAfterScalable = vi.fn()
            .mockRejectedValueOnce(error)
            .mockResolvedValueOnce(true)
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            scheduleReleaseRetry: (retry) => {
                retries.push(retry)
                return vi.fn()
            },
            releaseAfterScalable,
        })
        await controller.transition('maximum-compatibility')

        await expect(controller.transition('scalable-v3')).rejects.toBe(error)

        expect(controller.allowsEviction).toBe(false)
        expect(retries).toHaveLength(1)
        retries[0]()
        await vi.waitFor(() => expect(releaseAfterScalable).toHaveBeenCalledTimes(2))
        expect(controller.allowsEviction).toBe(true)
    })

    it('retries when the final generation-state lookup fails', async () => {
        const error = new Error('generation state chunk failed')
        const retries: Array<() => void> = []
        const canReleaseWorkingSet = vi.fn()
            .mockRejectedValueOnce(error)
            .mockResolvedValueOnce(true)
        const releaseAfterScalable = vi.fn(async () => true)
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            canReleaseWorkingSet,
            scheduleReleaseRetry: (retry) => {
                retries.push(retry)
                return vi.fn()
            },
            releaseAfterScalable,
        })
        await controller.transition('maximum-compatibility')

        await expect(controller.transition('scalable-v3')).rejects.toBe(error)
        expect(controller.allowsEviction).toBe(false)
        expect(retries).toHaveLength(1)

        retries[0]()
        await vi.waitFor(() => expect(releaseAfterScalable).toHaveBeenCalledOnce())
        expect(controller.allowsEviction).toBe(true)
    })

    it('stops automatic scalable release retries after repeated failures', async () => {
        const error = new Error('persistent materialization failure')
        const retries: Array<() => void> = []
        const releaseAfterScalable = vi.fn(async () => {
            throw error
        })
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            scheduleReleaseRetry: (retry) => {
                retries.push(retry)
                return vi.fn()
            },
            onReleaseRetryError: vi.fn(),
            releaseAfterScalable,
        })
        await controller.transition('maximum-compatibility')
        await expect(controller.transition('scalable-v3')).rejects.toBe(error)

        for (let attempt = 0; attempt < 3; attempt++) {
            expect(retries).toHaveLength(attempt + 1)
            retries[attempt]()
            await vi.waitFor(() =>
                expect(releaseAfterScalable).toHaveBeenCalledTimes(attempt + 2),
            )
        }

        expect(retries).toHaveLength(3)
        expect(controller.allowsEviction).toBe(false)
    })

    it('does not publish a stale scalable release after maximum re-entry', async () => {
        const releaseGate = deferred<void>()
        const retries: Array<() => void> = []
        const releaseAfterScalable = vi.fn(async (isCurrent: () => boolean) => {
            await releaseGate.promise
            return isCurrent()
        })
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            enterMaximumCompatibility: vi.fn(async () => undefined),
            scheduleReleaseRetry: (retry) => {
                retries.push(retry)
                return vi.fn()
            },
            releaseAfterScalable,
        })
        await controller.transition('maximum-compatibility')
        const scalable = controller.transition('scalable-v3')
        await vi.waitFor(() => expect(releaseAfterScalable).toHaveBeenCalledOnce())

        await controller.transition('maximum-compatibility')
        releaseGate.resolve(undefined)
        await scalable

        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        expect(retries).toHaveLength(0)
    })

    it('rearms a deferred scalable release when maximum re-entry fails', async () => {
        let generationActive = true
        const retries: Array<() => void> = []
        const cancellations: Array<ReturnType<typeof vi.fn>> = []
        const maximumError = new Error('maximum retry failed')
        const enterMaximumCompatibility = vi.fn()
            .mockResolvedValueOnce(undefined)
            .mockRejectedValueOnce(maximumError)
        const releaseAfterScalable = vi.fn()
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            enterMaximumCompatibility,
            canReleaseWorkingSet: () => !generationActive,
            scheduleReleaseRetry: (retry) => {
                retries.push(retry)
                const cancel = vi.fn()
                cancellations.push(cancel)
                return cancel
            },
            releaseAfterScalable,
        })
        await controller.transition('maximum-compatibility')
        await controller.transition('scalable-v3')

        await expect(controller.transition('maximum-compatibility')).rejects.toBe(maximumError)

        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(false)
        expect(cancellations[0]).toHaveBeenCalledOnce()
        expect(retries).toHaveLength(2)

        generationActive = false
        retries[1]()
        await vi.waitFor(() => expect(releaseAfterScalable).toHaveBeenCalledOnce())
        expect(controller.allowsEviction).toBe(true)
    })

    it('cancels a pending scalable release retry on maximum re-entry and initialization', async () => {
        const retries: Array<() => void> = []
        const cancellations: Array<ReturnType<typeof vi.fn>> = []
        const releaseAfterScalable = vi.fn()
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: vi.fn(async () => undefined),
            canReleaseWorkingSet: () => false,
            scheduleReleaseRetry: (retry) => {
                retries.push(retry)
                const cancel = vi.fn()
                cancellations.push(cancel)
                return cancel
            },
            releaseAfterScalable,
        })
        await controller.transition('maximum-compatibility')
        await controller.transition('scalable-v3')

        await controller.transition('maximum-compatibility')
        expect(cancellations[0]).toHaveBeenCalledOnce()
        retries[0]()
        await Promise.resolve()
        await Promise.resolve()
        expect(releaseAfterScalable).not.toHaveBeenCalled()

        await controller.transition('scalable-v3')
        controller.initialize('maximum-compatibility')
        expect(cancellations[1]).toHaveBeenCalledOnce()
        retries[1]()
        await Promise.resolve()
        await Promise.resolve()

        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        expect(releaseAfterScalable).not.toHaveBeenCalled()
    })

    it('initializes the boot profile without running transition side effects', () => {
        const enterMaximumCompatibility = vi.fn(async () => undefined)
        const persistBeforeEviction = vi.fn(async () => undefined)
        const setEvictionAllowed = vi.fn()
        const controller = createPluginCompatibilityController({
            enterMaximumCompatibility,
            persistBeforeEviction,
            setEvictionAllowed,
        })

        controller.initialize('maximum-compatibility')

        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        expect(setEvictionAllowed).toHaveBeenCalledWith(false)
        expect(enterMaximumCompatibility).not.toHaveBeenCalled()
        expect(persistBeforeEviction).not.toHaveBeenCalled()
    })

    it('keeps eviction blocked until full compatibility persistence resolves', async () => {
        const pending = deferred<void>()
        const controller = createPluginCompatibilityController(() => pending.promise)
        await controller.transition('maximum-compatibility')

        const transition = controller.transition('scalable-v3')

        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(false)
        pending.resolve(undefined)
        await transition
        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(true)
    })

    it('preserves maximum compatibility after failed persistence and permits retry', async () => {
        const error = new Error('persistence failed')
        const persist = vi.fn().mockRejectedValueOnce(error).mockResolvedValueOnce(undefined)
        const controller = createPluginCompatibilityController(persist)
        await controller.transition('maximum-compatibility')

        await expect(controller.transition('scalable-v3')).rejects.toBe(error)
        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)

        await expect(controller.transition('scalable-v3')).resolves.toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(true)
    })

    it('does not persist same-profile transitions', async () => {
        const persist = vi.fn(async () => undefined)
        const controller = createPluginCompatibilityController(persist)

        await controller.transition('scalable-v3')
        await controller.transition('maximum-compatibility')
        await controller.transition('maximum-compatibility')

        expect(persist).not.toHaveBeenCalled()
    })

    it('keeps maximum compatibility when v2.1 is re-enabled during persistence', async () => {
        const pending = deferred<void>()
        const persist = vi.fn(() => pending.promise)
        const controller = createPluginCompatibilityController(persist)
        await controller.transition('maximum-compatibility')

        const disabling = controller.transition('scalable-v3')
        let enablingSettled = false
        const enabling = controller.transition('maximum-compatibility').then((profile) => {
            enablingSettled = true
            return profile
        })
        await Promise.resolve()

        expect(controller.profile).toBe('scalable-v3')
        expect(enablingSettled).toBe(false)
        pending.resolve(undefined)

        await expect(disabling).resolves.toBe('scalable-v3')
        await expect(enabling).resolves.toBe('maximum-compatibility')
        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        expect(persist).toHaveBeenCalledTimes(1)
    })

    it('persists a detached full candidate containing inactive v2.1 mutations', async () => {
        const compatibilityDatabase = {
            characters: [
                { chaId: 'active', name: 'Active' },
                { chaId: 'inactive', name: 'Before' },
            ],
        }
        const liveDatabase = new Proxy(compatibilityDatabase, {
            get: (target, property) => Reflect.get(target, property),
            set: (target, property, value) => Reflect.set(target, property, value),
        })
        const replace = vi.fn(async () => undefined)
        const persist = createFullCompatibilityPersistence(
            () => structuredClone(compatibilityDatabase),
            replace,
        )
        liveDatabase.characters[1].name = 'Changed by v2.1'

        await persist()
        liveDatabase.characters[1].name = 'Changed after capture'

        expect(replace).toHaveBeenCalledWith(
            {
                characters: [
                    { chaId: 'active', name: 'Active' },
                    { chaId: 'inactive', name: 'Changed by v2.1' },
                ],
            },
            'plugin-profile-change',
        )
    })

    it('serializes a late v2 unload before a newer v2.1 runtime load', async () => {
        const unloadStarted = deferred<void>()
        const finishUnload = deferred<void>()
        const loadedV2: string[] = []
        const loadedV3: string[] = []
        let activeStages = 0
        let overlapped = false
        const controller = createPluginCompatibilityController(async () => undefined)
        await controller.transition('maximum-compatibility')
        const load = createPluginLoadOrchestrator<string>({
            controller,
            loadV2: async (plugins) => {
                activeStages++
                if (activeStages > 1) overlapped = true
                try {
                    if (plugins.length === 0) {
                        unloadStarted.resolve(undefined)
                        await finishUnload.promise
                    }
                    loadedV2.splice(0, loadedV2.length, ...plugins)
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

        const disabling = load({ nextProfile: 'scalable-v3', pluginV2: [], pluginV3: ['old-v3'] })
        await unloadStarted.promise
        let enablingSettled = false
        const enabling = load({
            nextProfile: 'maximum-compatibility',
            pluginV2: ['enabled-v2.1'],
            pluginV3: ['new-v3'],
        }).then(() => {
            enablingSettled = true
        })
        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        await Promise.resolve()
        expect(enablingSettled).toBe(false)
        finishUnload.resolve(undefined)
        await disabling
        await enabling

        expect(overlapped).toBe(false)
        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        expect(loadedV2).toEqual(['enabled-v2.1'])
        expect(loadedV3).toEqual(['new-v3'])
    })

    it('serializes an in-flight v3 load before the latest plugin runtime', async () => {
        const oldV3Started = deferred<void>()
        const finishOldV3 = deferred<void>()
        const loadedV2: string[] = []
        const loadedV3: string[] = []
        let activeStages = 0
        let overlapped = false
        const controller = createPluginCompatibilityController(async () => undefined)
        const load = createPluginLoadOrchestrator<string>({
            controller,
            loadV2: async (plugins) => {
                activeStages++
                if (activeStages > 1) overlapped = true
                loadedV2.splice(0, loadedV2.length, ...plugins)
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

        const oldLoad = load({
            nextProfile: 'scalable-v3',
            pluginV2: [],
            pluginV3: ['old-v3'],
        })
        await oldV3Started.promise
        let latestSettled = false
        const latestLoad = load({
            nextProfile: 'maximum-compatibility',
            pluginV2: ['latest-v2.1'],
            pluginV3: ['latest-v3'],
        }).then(() => {
            latestSettled = true
        })

        expect(controller.profile).toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(false)
        await Promise.resolve()
        expect(latestSettled).toBe(false)
        finishOldV3.resolve(undefined)
        await oldLoad
        await latestLoad

        expect(overlapped).toBe(false)
        expect(loadedV2).toEqual(['latest-v2.1'])
        expect(loadedV3).toEqual(['latest-v3'])
        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
    })

    it('continues queued plugin loads after an earlier operation rejects', async () => {
        const failingV3Started = deferred<void>()
        const rejectFailingV3 = deferred<void>()
        const loadedV3: string[] = []
        const error = new Error('v3 unload failed')
        let activeStages = 0
        let overlapped = false
        const controller = createPluginCompatibilityController(async () => undefined)
        const load = createPluginLoadOrchestrator<string>({
            controller,
            loadV2: async () => undefined,
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

        const failingLoad = load({
            nextProfile: 'scalable-v3',
            pluginV2: [],
            pluginV3: ['failing-v3'],
        })
        await failingV3Started.promise
        let recoverySettled = false
        const recoveryLoad = load({
            nextProfile: 'maximum-compatibility',
            pluginV2: ['enabled-v2.1'],
            pluginV3: ['recovered-v3'],
        }).then(() => {
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
        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
    })

    it('routes plugin writes safely while the maximum snapshot persistence is pending', async () => {
        const persistenceStarted = deferred<void>()
        const allowPersistence = deferred<void>()
        const persistenceComplete = deferred<void>()
        let liveDatabase = {
            characters: [
                { type: 'character', chaId: 'active', name: 'Active', chats: [] },
                { type: 'character', chaId: 'inactive', name: 'Before', chats: [] },
            ],
            botPresets: [],
        } as unknown as Database
        let persistentDatabase = structuredClone(liveDatabase)
        const applyMaximumUpdate = vi.fn(async (update: Record<string, unknown>) => {
            Object.assign(liveDatabase, structuredClone(update))
        })
        const controller = createPluginCompatibilityController({
            persistBeforeEviction: async () => {
                const candidate = structuredClone(liveDatabase)
                persistenceStarted.resolve(undefined)
                await allowPersistence.promise
                persistentDatabase = candidate
                liveDatabase = structuredClone(candidate)
                persistenceComplete.resolve(undefined)
            },
        })
        await controller.transition('maximum-compatibility')
        const access = createPluginDatabaseAccess({
            store: {} as PersistentDataStore,
            flushPendingData: vi.fn(async () => undefined),
            getCompatibilityDatabase: () => liveDatabase,
            getCompatibilityProfile: () => controller.profile,
            getSelectedCharacterId: () => liveDatabase.characters[0]?.chaId ?? null,
            captureSelectedConversationTarget: () => null,
            acquireCompleteConversation: vi.fn(),
            refreshSelectedConversationAfterReplacement: vi.fn(),
            replacePersistentCompleteCharacter: vi.fn(),
            replacePersistentConversation: vi.fn(),
            reportIdentityReplacementRejected: vi.fn(),
            getNavigationGeneration: () => 0,
            applyCompatibilityDatabaseLite: vi.fn(),
            applyCompatibilityDatabase: applyMaximumUpdate,
            readPluginStorageSnapshot: vi.fn(async () => ({})),
            mutatePluginStorage: vi.fn(async () => undefined),
            invalidatePluginStorage: vi.fn(),
            materializeDatabaseSnapshot: async () => {
                await persistenceComplete.promise
                return {
                    database: structuredClone(persistentDatabase),
                    revision: 1,
                    mutationGeneration: 0,
                }
            },
            replacePersistentDatabase: async (database) => {
                persistentDatabase = structuredClone(database)
                liveDatabase = structuredClone(database)
            },
            snapshot: structuredClone,
        })

        const scalable = controller.transition('scalable-v3')
        await persistenceStarted.promise
        const characters = structuredClone(liveDatabase.characters)
        characters[1].name = 'Updated safely'
        const pluginWrite = access.setDatabase({ characters }, ['characters'])
        let pluginWriteSettled = false
        void pluginWrite.then(() => {
            pluginWriteSettled = true
        })
        await Promise.resolve()

        expect(controller.profile).toBe('scalable-v3')
        expect(pluginWriteSettled).toBe(false)
        expect(applyMaximumUpdate).not.toHaveBeenCalled()

        allowPersistence.resolve(undefined)
        await scalable
        await pluginWrite

        expect(persistentDatabase.characters[1].name).toBe('Updated safely')
        expect(liveDatabase.characters[1].name).toBe('Updated safely')
    })

    it('allows an awaited reentrant plugin load to queue without deadlocking its loader', async () => {
        const events: string[] = []
        const nestedComplete = deferred<void>()
        const guard = createPluginLoadReentrancyGuard((error) => {
            throw error
        })
        const controller = createPluginCompatibilityController(async () => undefined)
        let load!: ReturnType<typeof createPluginLoadOrchestrator<string>>
        load = createPluginLoadOrchestrator<string>({
            controller,
            loadV2: async (plugins) => {
                events.push(`start:${plugins.join(',')}`)
                await guard.runEvaluation(async () => {
                    if (plugins.includes('outer')) {
                        await guard.settle(load({
                            nextProfile: 'scalable-v3',
                            pluginV2: ['nested'],
                            pluginV3: [],
                        }))
                        events.push('outer-resumed')
                    }
                })
                events.push(`end:${plugins.join(',')}`)
                if (plugins.includes('nested')) nestedComplete.resolve(undefined)
            },
            loadV3: vi.fn(async () => undefined),
        })

        await load({ nextProfile: 'scalable-v3', pluginV2: ['outer'], pluginV3: [] })
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
        const controller = createPluginCompatibilityController(async () => undefined)
        const unloads = new Set<() => Promise<void>>([
            async () => {
                await guard.settle(load({
                    nextProfile: 'maximum-compatibility',
                    pluginV2: ['nested'],
                    pluginV3: [],
                }))
                events.push('unload-resumed')
            },
        ])
        let load!: ReturnType<typeof createPluginLoadOrchestrator<string>>
        load = createPluginLoadOrchestrator<string>({
            controller,
            loadV2: async (plugins, isCurrent) => {
                events.push(`start:${plugins.join(',')}`)
                if (plugins.length === 0) {
                    const current = await runPluginUnloadCallbacks(
                        unloads,
                        isCurrent,
                        guard,
                    )
                    if (!current) return
                }
                events.push(`end:${plugins.join(',')}`)
                if (plugins.includes('nested')) nestedComplete.resolve(undefined)
            },
            loadV3: vi.fn(async () => undefined),
        })

        controller.initialize('maximum-compatibility')
        await load({ nextProfile: 'scalable-v3', pluginV2: [], pluginV3: [] })
        await nestedComplete.promise

        expect(events).toEqual([
            'start:',
            'unload-resumed',
            'start:nested',
            'end:nested',
        ])
    })

    it('attaches a rejection handler immediately to an eagerly started maximum transition', async () => {
        const previousLoad = deferred<void>()
        const maximumError = new Error('maximum materialization failed')
        let rejectionHandlerAttached = false
        const observedMaximumTransition = {
            then<TResult1 = PluginCompatibilityProfile, TResult2 = never>(
                onfulfilled?: ((value: PluginCompatibilityProfile) => TResult1 | PromiseLike<TResult1>) | null,
                onrejected?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
            ) {
                rejectionHandlerAttached = typeof onrejected === 'function'
                return Promise.reject(maximumError).then(onfulfilled, onrejected)
            },
        } as Promise<PluginCompatibilityProfile>
        const controller = {
            profile: 'scalable-v3',
            allowsEviction: true,
            initialize: vi.fn(),
            transition: vi.fn((profile: PluginCompatibilityProfile) =>
                profile === 'maximum-compatibility'
                    ? observedMaximumTransition
                    : Promise.resolve(profile),
            ),
        } satisfies PluginCompatibilityController
        const load = createPluginLoadOrchestrator<string>({
            controller,
            loadV2: vi.fn(async () => undefined),
            loadV3: vi.fn(async (plugins) => {
                if (plugins.includes('previous')) await previousLoad.promise
            }),
        })
        const first = load({
            nextProfile: 'scalable-v3',
            pluginV2: [],
            pluginV3: ['previous'],
        })
        await vi.waitFor(() => expect(controller.transition).toHaveBeenCalledOnce())

        const maximum = load({
            nextProfile: 'maximum-compatibility',
            pluginV2: ['v2.1'],
            pluginV3: [],
        })
        const attachedImmediately = rejectionHandlerAttached
        previousLoad.resolve(undefined)

        await first
        await expect(maximum).rejects.toBe(maximumError)
        expect(attachedImmediately).toBe(true)
    })
})
