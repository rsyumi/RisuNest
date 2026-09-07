import { afterEach, describe, expect, it, vi } from 'vitest'

vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/,
    hasher: vi.fn(async () => 'hash'),
    parseMarkdownSafe: (value: string) => value,
    ParseMarkdown: vi.fn(async (value: string) => value),
    risuChatParser: (value: string) => value,
}))

import { DBState } from '../stores.svelte'
import { getDatabase, setDatabaseLite, type Database } from '../storage/database.svelte'
import { getV2PluginAPIs, pluginCompatibility, pluginStorageStore, pluginV2 } from './plugins.svelte'

afterEach(() => {
    pluginCompatibility.initialize('scalable-v3')
    pluginStorageStore.invalidate()
    pluginStorageStore.setEvictionAllowed(true)
})

describe('live V2 plugin storage synchronization', () => {
    it.each(['display', 'input', 'output', 'process'] as const)(
        'registers and removes both spellings of the %s script mode',
        (mode) => {
            const api = getV2PluginAPIs()
            const handler = vi.fn((text: string) => text)
            const key = `edit${mode}` as const
            api.addRisuScriptHandler(key, handler)
            expect(pluginV2[key].has(handler)).toBe(true)
            api.removeRisuScriptHandler(mode, handler)
            expect(pluginV2[key].has(handler)).toBe(false)
            api.addRisuScriptHandler(mode, handler)
            api.removeRisuScriptHandler(key, handler)
            expect(pluginV2[key].has(handler)).toBe(false)
            expect(() =>
                api.addRisuScriptHandler('secret-fixture' as never, handler),
            ).toThrow('addRisuScriptHandler: mode must be')
        },
    )
    it('reads a detached storage value without snapshotting unrelated database fields', () => {
        const unrelated = vi.fn(() => ({ payload: 'unrelated' }))
        const database = {
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: { requested: { items: ['saved'] } },
        } as unknown as Database
        Object.defineProperty(database, 'unrelated', {
            enumerable: true,
            get: unrelated,
        })
        setDatabaseLite(database)
        unrelated.mockClear()

        const storage = getV2PluginAPIs().pluginStorage
        const result = storage.getItem('requested') as { items: string[] }
        result.items.push('detached')
        expect(storage.getItem('missing')).toBeNull()
        expect(getDatabase().pluginCustomStorage.requested).toEqual({
            items: ['saved'],
        })
        expect(unrelated).not.toHaveBeenCalled()
    })

    it.each(['custom-property', 'explicit-storage'] as const)(
        'keeps nested reactive %s writes readable by V3',
        async (access) => {
            setDatabaseLite({
                characters: [],
                plugins: [],
                botPresets: [],
                pluginCustomStorage: { fixture: { nested: { count: 1 } } },
            } as unknown as Database)
            pluginStorageStore.preloadCompatibilityValues({ fixture: { nested: { count: 1 } } })
            const database = getV2PluginAPIs().getDatabase() as any
            const value =
                access === 'custom-property'
                    ? database.fixture
                    : database.pluginCustomStorage.fixture

            expect(() => {
                value.nested.count = 2
            }).not.toThrow()

            expect(getDatabase().pluginCustomStorage.fixture).toEqual({ nested: { count: 2 } })
            expect(DBState.db.pluginCustomStorage.fixture).toEqual({ nested: { count: 2 } })
            await expect(pluginStorageStore.getItem('fixture')).resolves.toEqual({
                nested: { count: 2 },
            })
        },
    )

    it('keeps nested reactive array edits readable by V3', async () => {
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: { fixture: { items: ['first'] } },
        } as unknown as Database)
        pluginStorageStore.preloadCompatibilityValues({ fixture: { items: ['first'] } })
        const database = getV2PluginAPIs().getDatabase() as any

        expect(() => {
            database.fixture.items.push('second')
        }).not.toThrow()

        expect(getDatabase().pluginCustomStorage.fixture).toEqual({
            items: ['first', 'second'],
        })
        await expect(pluginStorageStore.getItem('fixture')).resolves.toEqual({
            items: ['first', 'second'],
        })
    })

    it('preserves trailing holes in nested reactive arrays', async () => {
        const items = new Array<string>(3)
        items[0] = 'first'
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: { fixture: { items } },
        } as unknown as Database)
        pluginStorageStore.preloadCompatibilityValues({ fixture: { items } })
        const database = getV2PluginAPIs().getDatabase() as any

        expect(() => {
            database.fixture.items[0] = 'changed'
        }).not.toThrow()

        const cached = (await pluginStorageStore.getItem('fixture')) as {
            items: string[]
        }
        expect(cached.items).toHaveLength(3)
        expect(1 in cached.items).toBe(false)
        expect(2 in cached.items).toBe(false)
    })

    it('returns detached V3 cache values', async () => {
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: { fixture: { nested: { count: 1 } } },
        } as unknown as Database)
        pluginStorageStore.preloadCompatibilityValues({ fixture: { nested: { count: 1 } } })

        const cached = (await pluginStorageStore.getItem('fixture')) as {
            nested: { count: number }
        }
        cached.nested.count = 9

        expect(getDatabase().pluginCustomStorage.fixture).toEqual({ nested: { count: 1 } })
        await expect(pluginStorageStore.getItem('fixture')).resolves.toEqual({
            nested: { count: 1 },
        })
    })

    it('keeps native structured-clone rejection for unsupported symbols', () => {
        expect(() =>
            pluginStorageStore.synchronizeCompatibilityMutation({
                type: 'set',
                key: 'unsupported',
                value: Symbol('unsupported'),
            }),
        ).toThrowError(expect.objectContaining({ name: 'DataCloneError' }))
    })

    it('accepts explicit storage replacement with live reactive values', async () => {
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: { fixture: { nested: { count: 1 } } },
        } as unknown as Database)
        pluginStorageStore.preloadCompatibilityValues({ fixture: { nested: { count: 1 } } })
        const database = getV2PluginAPIs().getDatabase() as any
        const liveValue = DBState.db.pluginCustomStorage.fixture

        expect(() => {
            database.pluginCustomStorage = { replacement: liveValue }
        }).not.toThrow()

        expect(getDatabase().pluginCustomStorage).toEqual({
            replacement: { nested: { count: 1 } },
        })
        await expect(pluginStorageStore.getItem('replacement')).resolves.toEqual({
            nested: { count: 1 },
        })
        await expect(pluginStorageStore.getItem('fixture')).resolves.toBeNull()
    })

    it('preserves own __proto__ storage through a live full resynchronization', async () => {
        const storage = JSON.parse(
            '{"__proto__":false,"fixture":{"nested":{"__proto__":false},"count":1}}',
        ) as Record<string, unknown>
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: storage,
        } as unknown as Database)
        pluginStorageStore.preloadCompatibilityValues(storage)
        const database = getV2PluginAPIs().getDatabase() as any

        expect(() => {
            database.pluginCustomStorage.fixture.count = 2
        }).not.toThrow()

        expect(Object.hasOwn(getDatabase().pluginCustomStorage, '__proto__')).toBe(true)
        await expect(pluginStorageStore.getItem('__proto__')).resolves.toBe(false)
        const cached = (await pluginStorageStore.getItem('fixture')) as {
            nested: Record<string, unknown>
            count: number
        }
        expect(Object.hasOwn(cached.nested, '__proto__')).toBe(true)
        expect(cached.nested.__proto__).toBe(false)
        expect(cached.count).toBe(2)
    })

    it('refreshes V3 cache when a maximum-profile database update writes a custom key', async () => {
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: { fixture: { count: 1 } },
        } as unknown as Database)
        pluginCompatibility.initialize('maximum-compatibility')
        pluginStorageStore.preloadCompatibilityValues({ fixture: { count: 1 } })

        getV2PluginAPIs().setDatabaseLite({ fixture: { count: 2 } })

        expect(getDatabase().pluginCustomStorage.fixture).toEqual({ count: 2 })
        await expect(pluginStorageStore.getItem('fixture')).resolves.toEqual({ count: 2 })
    })
})
