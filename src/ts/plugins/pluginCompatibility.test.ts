import { describe, expect, it, vi } from 'vitest'
import {
    createPluginCompatibilityController,
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
        const flush = vi.fn(async () => undefined)
        const controller = createPluginCompatibilityController(flush)

        const transition = controller.transition('maximum-compatibility')

        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)
        await transition
        expect(flush).not.toHaveBeenCalled()
    })

    it('keeps eviction blocked until the scalable transition flush resolves', async () => {
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

    it('preserves maximum compatibility after a failed flush and permits retry', async () => {
        const error = new Error('flush failed')
        const flush = vi.fn().mockRejectedValueOnce(error).mockResolvedValueOnce(undefined)
        const controller = createPluginCompatibilityController(flush)
        await controller.transition('maximum-compatibility')

        await expect(controller.transition('scalable-v3')).rejects.toBe(error)
        expect(controller.profile).toBe('maximum-compatibility')
        expect(controller.allowsEviction).toBe(false)

        await expect(controller.transition('scalable-v3')).resolves.toBe('scalable-v3')
        expect(controller.allowsEviction).toBe(true)
    })

    it('does not flush same-profile transitions', async () => {
        const flush = vi.fn(async () => undefined)
        const controller = createPluginCompatibilityController(flush)

        await controller.transition('scalable-v3')
        await controller.transition('maximum-compatibility')
        await controller.transition('maximum-compatibility')

        expect(flush).not.toHaveBeenCalled()
    })
})
