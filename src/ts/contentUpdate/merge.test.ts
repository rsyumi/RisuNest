import { describe, expect, it } from 'vitest'
import type { RisuModule } from '../process/modules'
import {
    CHARACTER_UPDATE,
    MODULE_UPDATE,
    UPDATE_BASE_EXTENSION_KEY,
    applyUpdate,
    baseSnapshot,
    findCharacterUpdateCandidates,
    findModuleUpdateCandidates,
    markModuleImport,
    planUpdate,
    summarizePlan,
    unresolvedItems,
    updateNameKey,
} from './merge'

const lore = (comment: string, content: string, id?: string) => ({
    key: comment.toLowerCase(), secondkey: '', insertorder: 100, comment, content,
    mode: 'normal', alwaysActive: false, selective: false, useRegex: false, ...(id ? { id } : {}),
})

describe('updateNameKey', () => {
    it.each([
        'Luna v1.1', 'Luna v1.2', 'Luna v1.0.0', 'Luna ver.3', 'Luna [v2]', 'Luna (2)', 'Luna - 버전 2',
        'Luna 1.2', 'Luna Beta', 'Luna (Beta)', 'Luna 베타', 'Luna Final', 'Luna v1.0 beta', 'Luna 1.2 수정판',
        'Luna v1.2a', 'LUNA', ' luna ',
    ])('reduces %j to the plain name', (name) => {
        expect(updateNameKey(name)).toBe('luna')
    })

    it('keeps numbers and words that are part of the name', () => {
        expect(updateNameKey('Luna 2')).toBe('luna2')
        expect(updateNameKey('Lunafix')).toBe('lunafix')
        expect(updateNameKey('Test')).toBe('test')
        expect(updateNameKey('v1.2')).toBe('v12')
    })
})

describe('update candidates', () => {
    it('lists characters with the same name key, most chats first', () => {
        const installed = [
            { id: 'a', name: 'Luna v1.0', count: 1 },
            { id: 'b', name: 'Luna Beta', count: 4 },
            { id: 'c', name: 'Luna 2', count: 9 },
        ]
        expect(findCharacterUpdateCandidates(installed, 'Luna v1.2').map((entry) => entry.id)).toEqual(['b', 'a'])
        expect(findCharacterUpdateCandidates(installed, '')).toEqual([])
    })

    it('ranks modules by file lineage, then name, then namespace', () => {
        const modules = [
            { id: 'by-namespace', name: 'Other', description: '', namespace: 'luna' },
            { id: 'by-name', name: 'Luna Lore v1', description: '' },
            { id: 'by-lineage', name: 'Renamed', description: '', updateLineage: 'file-id' },
            { id: '$persona', name: 'Luna Lore', description: '' },
            { id: 'mcp', name: 'Luna Lore', description: '', mcp: { url: 'x' } },
            { id: 'incoming', name: 'Luna Lore', description: '', updateLineage: 'file-id' },
        ] as RisuModule[]
        const incoming = { id: 'incoming', name: 'Luna Lore v2', description: '', namespace: 'luna', updateLineage: 'file-id' } as RisuModule
        expect(findModuleUpdateCandidates(modules, incoming).map((entry) => [entry.id, entry.reason])).toEqual([
            ['by-lineage', 'lineage'], ['by-name', 'name'], ['by-namespace', 'namespace'],
        ])
    })

    it('records the file id of an imported module once and drops a carried base', () => {
        const module = { id: 'file-id', name: 'M', description: '', updateBase: 'assets/old.json' } as RisuModule
        markModuleImport(module)
        expect(module.updateLineage).toBe('file-id')
        expect(module.updateBase).toBeUndefined()
        module.id = 'second-id'
        markModuleImport(module)
        expect(module.updateLineage).toBe('file-id')
    })
})

describe('planUpdate', () => {
    const base = { name: 'Luna', desc: 'old', personality: 'calm', scenario: 'a', globalLore: [lore('Town', 'town'), lore('Gone', 'bye')] }

    it('sorts each change by who made it', () => {
        const mine = { ...base, chaId: 'x', chats: [{ id: 'c' }], personality: 'brave', scenario: 'mine', globalLore: [lore('Town', 'town', 'm1'), lore('Gone', 'bye', 'm2'), lore('Own', 'own', 'm3')] }
        const theirs = { ...base, desc: 'new', scenario: 'theirs', chats: [], globalLore: [lore('Town', 'town!'), lore('Fresh', 'fresh')] }
        const plan = planUpdate(CHARACTER_UPDATE, mine as any, theirs as any, base as any)
        const status = Object.fromEntries(plan.items.map((item) => [item.key ? `${item.field}:${JSON.parse(item.key)[1]}` : item.field, [item.status, item.choice]]))
        expect(status).toEqual({
            desc: ['update', 'theirs'],
            personality: ['mine', 'mine'],
            scenario: ['conflict', null],
            'globalLore:Town': ['update', 'theirs'],
            'globalLore:Fresh': ['added', 'theirs'],
            'globalLore:Gone': ['removed', 'theirs'],
            'globalLore:Own': ['mineOnly', 'mine'],
        })
        expect(unresolvedItems(plan).map((item) => item.field)).toEqual(['scenario'])
        expect(summarizePlan(plan)).toMatchObject({ update: 2, mine: 1, conflict: 1, added: 1, removed: 1, mineOnly: 1, unresolved: 1 })
    })

    it('asks about every difference when the installed version is unknown', () => {
        const plan = planUpdate(CHARACTER_UPDATE, { name: 'Luna', desc: 'old', personality: 'calm' } as any, { name: 'Luna', desc: 'new', personality: 'calm' } as any, null)
        expect(plan.items.map((item) => [item.field, item.status, item.choice])).toEqual([['desc', 'differs', null]])
    })

    it('finds nothing to update in an identical import, with or without a base', () => {
        const mine = { ...base, chaId: 'x', creation_date: 1, globalLore: [lore('Town', 'town', 'id-1'), lore('Gone', 'bye', 'id-2')] }
        const theirs = { ...base, creation_date: 2, globalLore: [lore('Town', 'town', 'id-3'), lore('Gone', 'bye', 'id-4')] }
        expect(planUpdate(CHARACTER_UPDATE, mine as any, theirs as any, null).items).toEqual([])
        expect(planUpdate(CHARACTER_UPDATE, mine as any, theirs as any, base as any).items).toEqual([])
    })

    it('never offers the user data or the update bookkeeping', () => {
        const mine = { name: 'Luna', chats: [{ id: 'mine' }], notes: 'n', extentions: { [UPDATE_BASE_EXTENSION_KEY]: 'assets/a.json', risuRealmImportId: 'r1' } }
        const theirs = { name: 'Luna', chats: [], notes: '', extentions: { [UPDATE_BASE_EXTENSION_KEY]: 'assets/b.json', risuRealmImportId: 'r2' } }
        expect(planUpdate(CHARACTER_UPDATE, mine as any, theirs as any, null).items).toEqual([])
    })
})

describe('applyUpdate', () => {
    it('keeps user data, takes chosen sides and puts the user lore back where it was', () => {
        const base = { name: 'Luna', desc: 'old', scenario: 'a', globalLore: [lore('A', 'a'), lore('B', 'b')], extentions: { color: 'red' } }
        const mine = {
            ...base, chaId: 'x', chats: [{ id: 'chat' }], scenario: 'mine', creation_date: 1,
            globalLore: [lore('A', 'a', 'id-a'), lore('Own', 'own', 'id-own'), lore('B', 'b', 'id-b')],
            extentions: { color: 'red', mood: 'mine', [UPDATE_BASE_EXTENSION_KEY]: 'assets/old.json' },
        }
        const theirs = {
            ...base, chats: [], desc: 'new', scenario: 'theirs', creation_date: 2,
            globalLore: [lore('A', 'a2', 'id-new'), lore('B', 'b'), lore('C', 'c')],
            extentions: { color: 'blue', [UPDATE_BASE_EXTENSION_KEY]: 'assets/new.json' },
        }
        const plan = planUpdate(CHARACTER_UPDATE, mine as any, theirs as any, base as any)
        expect(() => applyUpdate(CHARACTER_UPDATE, mine as any, theirs as any, plan)).toThrow('scenario')
        const merged = applyUpdate(CHARACTER_UPDATE, mine as any, theirs as any, plan, { scenario: { value: 'written' } }) as any
        expect(merged).toMatchObject({ chaId: 'x', chats: [{ id: 'chat' }], desc: 'new', scenario: 'written', creation_date: 2 })
        expect(merged.globalLore.map((entry: any) => [entry.comment, entry.content, entry.id])).toEqual([
            ['A', 'a2', 'id-a'], ['Own', 'own', 'id-own'], ['B', 'b', 'id-b'], ['C', 'c', undefined],
        ])
        expect(merged.extentions).toEqual({ color: 'blue', mood: 'mine', [UPDATE_BASE_EXTENSION_KEY]: 'assets/new.json' })
    })

    it('keeps the order of whichever side reordered the list, and asks whose order when both did', () => {
        const regex = (comment: string) => ({ comment, type: 'editoutput', in: comment, out: '' })
        const plan = (base: string[], mine: string[], theirs: string[]) => {
            const [b, m, t] = [base, mine, theirs].map((list) => ({ name: 'Luna', customscript: list.map(regex) })) as any[]
            return { m, t, plan: planUpdate(CHARACTER_UPDATE, m, t, b) }
        }
        const merge = (lists: ReturnType<typeof plan>, choices = {}) =>
            applyUpdate(CHARACTER_UPDATE, lists.m, lists.t, lists.plan, choices).customscript.map((script: any) => script.comment)
        expect(merge(plan(['A', 'B', 'C'], ['C', 'A', 'B'], ['A', 'B', 'N', 'C']))).toEqual(['C', 'A', 'B', 'N'])
        expect(merge(plan(['A', 'B', 'C'], ['A', 'B', 'C', 'Own'], ['B', 'A', 'C']))).toEqual(['B', 'A', 'C', 'Own'])
        expect(plan(['A', 'B', 'C'], ['B', 'A', 'C'], ['B', 'A', 'C']).plan.items).toEqual([])

        const both = plan(['A', 'B', 'C'], ['B', 'A', 'C'], ['A', 'C', 'B'])
        expect(both.plan.items).toMatchObject([{ id: 'customscript', status: 'conflict', choice: null, mine: ['B', 'A', 'C'], theirs: ['A', 'C', 'B'] }])
        expect(merge(both, { customscript: 'mine' })).toEqual(['B', 'A', 'C'])
        expect(merge(both, { customscript: 'theirs' })).toEqual(['A', 'C', 'B'])
        // Without a base, who reordered it is unknown.
        expect(planUpdate(CHARACTER_UPDATE, both.m, both.t, null).items).toMatchObject([{ id: 'customscript', order: true, status: 'differs', choice: null }])
    })

    it('tells apart assets that share a name from one named like a second copy', () => {
        const assets = [['face', 'assets/1.png', 'png'], ['face', 'assets/2.png', 'png'], ['face#1', 'assets/3.png', 'png']]
        const mine = { id: 'm', name: 'M', description: '', assets }
        const plan = planUpdate(MODULE_UPDATE, mine as RisuModule, { ...mine, id: 't' } as RisuModule, null)
        expect(plan.items).toEqual([])
        expect(applyUpdate(MODULE_UPDATE, mine as RisuModule, { ...mine, id: 't' } as RisuModule, plan).assets).toEqual(assets)
    })

    it('keeps the installed module id and takes the new lineage', () => {
        const mine = { id: 'installed', name: 'M', description: 'old', updateBase: 'assets/a.json', updateLineage: 'one' } as RisuModule
        const theirs = { id: 'incoming', name: 'M', description: 'new', updateBase: 'assets/b.json', updateLineage: 'two' } as RisuModule
        const plan = planUpdate(MODULE_UPDATE, mine, theirs, null)
        expect(plan.items.map((item) => item.field)).toEqual(['description'])
        const merged = applyUpdate(MODULE_UPDATE, mine, theirs, plan, { description: 'theirs' })
        expect(merged).toEqual({ id: 'installed', name: 'M', description: 'new', updateBase: 'assets/a.json', updateLineage: 'two' })
    })
})

describe('baseSnapshot', () => {
    it('stores the import without user data or its own base key', () => {
        const snapshot = baseSnapshot(CHARACTER_UPDATE, {
            name: 'Luna', chaId: 'x', chats: [{ id: 'c' }], desc: 'd', missing: undefined,
            extentions: { color: 'red', [UPDATE_BASE_EXTENSION_KEY]: 'assets/a.json' },
        } as any)
        expect(snapshot).toEqual({ name: 'Luna', desc: 'd', extentions: { color: 'red' } })
    })
})
