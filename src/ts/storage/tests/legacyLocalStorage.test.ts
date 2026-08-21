import { describe, expect, it, vi } from 'vitest'
import { selectLegacyLocalStorage } from '../legacyLocalStorage'

vi.mock('localforage', () => ({
    default: { createInstance: vi.fn() },
}))
vi.mock('src/ts/platform', () => ({ isNodeServer: false }))
vi.mock('../nodeStorage', () => ({ NodeStorage: class {} }))
vi.mock('../opfsStorage', () => ({ OpfsStorage: class {} }))

describe('selectLegacyLocalStorage', () => {
    it('selects Node storage for the Node server', () => {
        const node = { getItem: vi.fn(), keys: vi.fn() }
        const opfs = { getItem: vi.fn(), keys: vi.fn() }
        const forage = { getItem: vi.fn(), keys: vi.fn() }

        expect(selectLegacyLocalStorage({
            isNodeServer: true,
            canUseOpfs: true,
            opfsEnabled: true,
            createNodeStorage: () => node,
            createOpfsStorage: () => opfs,
            createForageStorage: () => forage,
        })).toBe(node)
    })

    it('selects OPFS only when its capability and flag are enabled', () => {
        const opfs = { getItem: vi.fn(), keys: vi.fn() }
        const forage = { getItem: vi.fn(), keys: vi.fn() }
        const common = {
            isNodeServer: false,
            createNodeStorage: vi.fn(),
            createOpfsStorage: () => opfs,
            createForageStorage: () => forage,
        }

        expect(selectLegacyLocalStorage({
            ...common,
            canUseOpfs: true,
            opfsEnabled: true,
        })).toBe(opfs)
        expect(selectLegacyLocalStorage({
            ...common,
            canUseOpfs: false,
            opfsEnabled: true,
        })).toBe(forage)
        expect(selectLegacyLocalStorage({
            ...common,
            canUseOpfs: true,
            opfsEnabled: false,
        })).toBe(forage)
    })
})
