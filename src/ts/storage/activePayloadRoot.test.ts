import { describe, expect, test, vi } from 'vitest'
import { ActivePayloadRoot } from './activePayloadRoot'
import type { ActivePersistentTuple, PersistentDataStore } from './persistentDataStore'

describe('active payload root', () => {
    test('maps one coherent tuple to both blob and cold resolvers', async () => {
        const tuple: ActivePersistentTuple = {
            revision: 4, dataGeneration: 'data-4', payloadGeneration: 'payload_4',
        }
        const readActiveTuple = vi.fn(async () => tuple)
        const controller = new ActivePayloadRoot({ readActiveTuple } as unknown as PersistentDataStore)

        const refreshed = await controller.refresh()
        expect(refreshed).toEqual(tuple)
        expect(refreshed).not.toBe(tuple)
        expect(controller.current()).toBe(refreshed)
        expect(await controller.getActiveRoot()).toEqual({ kind: 'generation', id: 'payload_4' })
        expect(controller.getActiveColdRoot()).toEqual({ kind: 'generation', id: 'payload_4' })
        expect(readActiveTuple).toHaveBeenCalledOnce()
    })

    test('maps the reserved legacy payload root exactly', () => {
        const controller = new ActivePayloadRoot({} as PersistentDataStore)
        const tuple = { revision: 1, dataGeneration: 'legacy-data', payloadGeneration: 'legacy' }
        controller.install(tuple)
        expect(controller.current()).toEqual(tuple)
        expect(controller.current()).not.toBe(tuple)
        expect(controller.getActiveColdRoot()).toEqual({ kind: 'legacy' })
    })

    test('rejects unsafe generated payload roots before installation', () => {
        const controller = new ActivePayloadRoot({} as PersistentDataStore)
        expect(() => controller.install({
            revision: 1, dataGeneration: 'data', payloadGeneration: '../escape',
        })).toThrow(TypeError)
        expect(() => controller.current()).toThrow('not installed')
    })

    test('owns and freezes the installed tuple so caller mutation cannot redirect authority', async () => {
        const controller = new ActivePayloadRoot({} as PersistentDataStore)
        const tuple = { revision: 2, dataGeneration: 'data-2', payloadGeneration: 'safe_2' }
        controller.install(tuple)
        const current = controller.current()

        expect(current).toEqual(tuple)
        expect(current).not.toBe(tuple)
        expect(Object.isFrozen(current)).toBe(true)
        tuple.payloadGeneration = '../escape'
        expect(await controller.getActiveRoot()).toEqual({ kind: 'generation', id: 'safe_2' })
        expect(() => { current.payloadGeneration = 'other' }).toThrow(TypeError)
    })
})
