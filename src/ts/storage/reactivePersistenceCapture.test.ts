import { describe, expect, it, vi } from 'vitest'
import { createPersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'
import { createPersistentSaveObserverHarness } from './tests/persistentSaveObserverHarness.svelte'
import { canonicalJson, pluginStorageJson } from './saveCoordinatorHelpers'
import type { Database } from './database.svelte'

function fixture() {
    return createPersistentSaveObserverHarness({
        username: 'Before',
        setting: { nested: { count: 0 } },
        large: 'x'.repeat(6 * 1024 * 1024),
        pluginCustomStorage: { nested: { count: 0 } },
        botPresets: [{ name: 'Preset', nested: [1] }],
        characters: [{ chaId: 'synthetic', chats: [{ message: [{ data: 'hello' }] }] }],
    } as unknown as Database)
}
function capture(state: ReturnType<typeof fixture>, root = () => state.database) {
    return createPersistenceCanonicalCapture({
        root,
        pluginStorage: () => state.database.pluginCustomStorage,
        presets: () => state.database.botPresets,
        character: () => state.database.characters[state.selectedIndex] ?? null,
    })
}
function root(state: ReturnType<typeof fixture>) {
    const { characters: _, pluginCustomStorage: __, botPresets: ___, ...value } = state.database
    return value
}

describe('production reactive persistence captures', () => {
    it('does not cache plain or raw objects whose edits cannot invalidate a derived', () => {
        const raw = {
            root: { nested: { value: 1 } },
            storage: { a: { count: 1 } },
            presets: [1],
            character: { name: 'first' },
        }
        const cached = createPersistenceCanonicalCapture({
            root: () => raw.root,
            pluginStorage: () => raw.storage,
            presets: () => raw.presets,
            character: () => raw.character,
        })
        cached.root()
        cached.pluginStorage()
        cached.presets()
        cached.character()
        raw.root.nested.value++
        raw.storage.a.count++
        raw.presets.push(2)
        raw.character.name = 'second'
        expect(cached.root()).toBe(canonicalJson(raw.root))
        expect(cached.pluginStorage()!.json).toBe(pluginStorageJson(raw.storage))
        expect(cached.presets()).toBe(canonicalJson(raw.presets))
        expect(cached.character()).toBe(canonicalJson(raw.character))
    })

    it('observes immediate nested edits without waiting for effects and creates only changed root fields', () => {
        const state = fixture()
        const cached = capture(state)
        const before = cached.root()
        ;(state.database as unknown as Record<string, any>).setting.nested.count++
        const after = cached.root()
        expect(after).toBe(canonicalJson(root(state)))
        expect(cached.diffRoot(before, after)).toEqual([
            { type: 'set', key: 'setting', value: { nested: { count: 1 } } },
        ])
        expect(JSON.stringify(cached.diffRoot(before, after)).length).toBeLessThan(256)
        delete (state.database as unknown as Record<string, any>).setting
        expect(cached.diffRoot(after, cached.root())).toEqual([{ type: 'delete', key: 'setting' }])
    })

    it('does not visit unchanged fields during a setting save', () => {
        const state = fixture()
        const reads = vi.fn()
        const cached = capture(
            state,
            () =>
                new Proxy(state.database, {
                    get(target, key, receiver) {
                        reads(key)
                        return Reflect.get(target, key, receiver)
                    },
                }),
        )
        cached.root()
        reads.mockClear()
        state.database.username = 'After'
        cached.root()
        expect(reads.mock.calls.map(([key]) => key)).not.toContain('large')
        expect(reads.mock.calls.map(([key]) => key)).not.toContain('setting')
    })

    it('keeps plugin order, array holes, selection and detached values correct', () => {
        const state = fixture()
        const cached = capture(state)
        cached.pluginStorage()
        cached.presets()
        cached.character()
        state.database.pluginCustomStorage.nested.count++
        state.database.pluginCustomStorage['__proto__'] = { own: true }
        expect(cached.pluginStorage()!.json).toBe(
            pluginStorageJson(state.database.pluginCustomStorage),
        )
        const detached = cached.pluginStorage()!.value
        detached.nested.count = 999
        expect(cached.pluginStorage()!.value.nested.count).toBe(1)
        delete state.database.botPresets[0]
        expect(cached.presets()).toBe(canonicalJson(state.database.botPresets))
        state.database.characters[0].chats[0].message[0].data += '!'
        expect(cached.character()).toBe(canonicalJson(state.database.characters[0]))
        state.selectedIndex = 1
        expect(cached.character()).toBeNull()
    })

    it('refreshes getters and callable hooks backed by non-reactive values', () => {
        const state = fixture()
        let outside = 1
        state.database = {
            ...state.database,
            get getter() {
                return outside
            },
        } as Database
        ;(state.database as unknown as Record<string, any>).setting = {
            toJSON() {
                return outside
            },
        }
        state.database.pluginCustomStorage.hook = {
            toJSON(key: string) {
                return `${key}:${outside}`
            },
        }
        const cached = capture(state)
        expect(cached.root()).toBe(canonicalJson(root(state)))
        outside++
        expect(cached.root()).toBe(canonicalJson(root(state)))
        expect(cached.pluginStorage()!.json).toBe(
            pluginStorageJson(state.database.pluginCustomStorage),
        )
    })
})
