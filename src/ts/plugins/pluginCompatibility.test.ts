import { describe, expect, it, vi } from 'vitest'
import {
    createFullCompatibilityPersistence,
    createPluginCompatibilityController,
    createPluginLoadOrchestrator,
    selectPluginCompatibilityProfile,
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

describe('plugin compatibility profiles', () => {
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

        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        await transition
        expect(persist).not.toHaveBeenCalled()
    })

    it('keeps eviction blocked until full compatibility persistence resolves', async () => {
        const pending = deferred<void>()
        const controller = createPluginCompatibilityController(() => pending.promise)
        await controller.transition('maximum-compatibility')

        const transition = controller.transition('scalable-v3')

        expect(controller.profile).toBe('maximum-compatibility')
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

        expect(controller.profile).toBe('maximum-compatibility')
        expect(enablingSettled).toBe(false)
        pending.resolve(undefined)

        await expect(disabling).resolves.toBe('maximum-compatibility')
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

        expect(controller.profile).toBe('maximum-compatibility')
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
})
