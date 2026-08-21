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

        expect(await controller.refresh()).toBe(tuple)
        expect(controller.current()).toBe(tuple)
        expect(await controller.getActiveRoot()).toEqual({ kind: 'generation', id: 'payload_4' })
        expect(controller.getActiveColdRoot()).toEqual({ kind: 'generation', id: 'payload_4' })
        expect(readActiveTuple).toHaveBeenCalledOnce()
    })

    test('maps the reserved legacy payload root exactly', () => {
        const controller = new ActivePayloadRoot({} as PersistentDataStore)
        const tuple = { revision: 1, dataGeneration: 'legacy-data', payloadGeneration: 'legacy' }
        controller.install(tuple)
        expect(controller.current()).toBe(tuple)
        expect(controller.getActiveColdRoot()).toEqual({ kind: 'legacy' })
    })

    test('rejects unsafe generated payload roots before installation', () => {
        const controller = new ActivePayloadRoot({} as PersistentDataStore)
        expect(() => controller.install({
            revision: 1, dataGeneration: 'data', payloadGeneration: '../escape',
        })).toThrow(TypeError)
        expect(() => controller.current()).toThrow('not installed')
    })
})
