import { describe, expect, it } from 'vitest'
import type { Database, botPreset, Chat } from './database.svelte'
import {
    createPresetMirrorView, deriveEffectivePersonaMirrors, deriveEffectivePresetMirrors, deriveEffectiveToggleVariables,
    derivePresetMirrorView, flushEffectivePersonaEdits, flushEffectivePresetEdits, flushEffectiveToggleEdits,
    flushPresetMirrorView, getEffectivePresetId, getExplicitGlobalChatVariables, prepareImportedIdentityState,
    presetMirrorMap, setEffectivePresetBinding, setEffectivePresetOverride, translateRootUnitIntents,
} from './effectiveIdentityState'

function database(): Database {
    return {
        botPresetsId: 0, botPresets: [
            { id: 'preset-a', name: 'A', mainPrompt: 'A', openAIKey: 'opaque', autoSuggestPrefix: 'prefix-a' },
            { id: 'preset-b', name: 'B', mainPrompt: 'B', autoSuggestPrefix: 'prefix-b' },
        ], selectedPersona: 0, personas: [
            { id: 'persona-a', name: 'A', icon: 'a', personaPrompt: 'a', note: 'a' },
            { id: 'persona-b', name: 'B', icon: 'b', personaPrompt: 'b', note: 'b' },
        ], characters: [], globalChatVariables: { toggle_a: 'explicit', other: 'shared', extension: 'opaque' },
        openAIKey: 'independent', mainPrompt: 'stale',
    } as unknown as Database
}
function apply(db: Database, preset: botPreset): void {
    const fields = db as unknown as Record<string, unknown>
    const record = preset as unknown as Record<string, unknown>
    for (const [key, field] of Object.entries(presetMirrorMap)) {
        if (Object.hasOwn(record, field)) fields[key] = structuredClone(record[field])
    }
}
function derive(db: Database): void {
    deriveEffectivePresetMirrors(db, apply)
    deriveEffectivePersonaMirrors(db)
    deriveEffectiveToggleVariables(db)
}

describe('authoritative identity records and mirrors', () => {
    it('derives on load without rewriting records or the independent API key', () => {
        const db = database()
        const records = structuredClone(db.botPresets)
        derive(db)
        expect(db.mainPrompt).toBe('A')
        expect(db.username).toBe('A')
        flushEffectivePresetEdits(db)
        flushEffectivePersonaEdits(db)
        expect(db.botPresets).toEqual(records)
        expect(db.openAIKey).toBe('independent')
    })
    it('attributes a preceding edit to the previous effective record after selection changes', () => {
        const db = database()
        derive(db)
        db.mainPrompt = 'edited A'
        db.botPresetsId = 1
        flushEffectivePresetEdits(db)
        deriveEffectivePresetMirrors(db, apply)
        expect(db.botPresets[0].mainPrompt).toBe('edited A')
        expect(db.botPresets[1].mainPrompt).toBe('B')
        expect(db.mainPrompt).toBe('B')
    })
    it('keeps remote record fields when the old effective mirror was untouched', () => {
        const db = database()
        derive(db)
        db.botPresets[0].mainPrompt = 'remote A'
        flushEffectivePresetEdits(db)
        deriveEffectivePresetMirrors(db, apply)
        expect(db.mainPrompt).toBe('remote A')
        expect(db.botPresets[0].mainPrompt).toBe('remote A')
    })
    it('keeps the shared selection while a chain override receives edits and order changes', () => {
        const db = database()
        derive(db)
        setEffectivePresetOverride(db, 'preset-b', apply)
        expect(db.botPresetsId).toBe(0)
        expect(db.mainPrompt).toBe('B')
        db.mainPrompt = 'chain edit'
        db.botPresets.reverse()
        db.botPresetsId = 1
        flushEffectivePresetEdits(db)
        expect(db.botPresets.find(preset => preset.id === 'preset-b').mainPrompt).toBe('chain edit')
        expect(getEffectivePresetId(db)).toBe('preset-b')
        setEffectivePresetOverride(db, null, apply)
        expect(getEffectivePresetId(db)).toBe('preset-a')
        expect(db.mainPrompt).toBe('A')
    })
    it('clears a deleted chain override and derives the remaining explicit selection', () => {
        const db = database()
        derive(db)
        setEffectivePresetOverride(db, 'preset-b', apply)
        db.botPresets.splice(1, 1)
        const remaining = structuredClone(db.botPresets)
        deriveEffectivePresetMirrors(db, apply)
        expect(getEffectivePresetId(db)).toBe('preset-a')
        expect(db.botPresetsId).toBe(0)
        expect(db.mainPrompt).toBe('A')
        flushEffectivePresetEdits(db)
        expect(db.botPresets).toEqual(remaining)
        db.mainPrompt = 'remaining edit'
        flushEffectivePresetEdits(db)
        expect(db.botPresets[0].mainPrompt).toBe('remaining edit')
    })
    it('applies a conversation binding above the chain pick and falls through when its preset is missing', () => {
        const db = database()
        db.botPresets.push({ id: 'preset-c', name: 'C', mainPrompt: 'C' } as botPreset)
        derive(db)
        setEffectivePresetOverride(db, 'preset-c', apply)
        setEffectivePresetBinding(db, 'preset-b', apply)
        expect(getEffectivePresetId(db)).toBe('preset-b')
        expect(db.mainPrompt).toBe('B')
        // The chain's null clears only its own pick.
        setEffectivePresetOverride(db, null, apply)
        expect(getEffectivePresetId(db)).toBe('preset-b')
        db.mainPrompt = 'bound edit'
        flushEffectivePresetEdits(db)
        expect(db.botPresets.map(preset => preset.mainPrompt)).toEqual(['A', 'bound edit', 'C'])
        setEffectivePresetBinding(db, 'missing', apply)
        expect(getEffectivePresetId(db)).toBe('preset-a')
        expect(db.mainPrompt).toBe('A')
        setEffectivePresetOverride(db, 'preset-c', apply)
        setEffectivePresetBinding(db, 'preset-b', apply)
        db.botPresets.splice(1, 1)
        deriveEffectivePresetMirrors(db, apply)
        expect(getEffectivePresetId(db)).toBe('preset-c')
        expect(db.mainPrompt).toBe('C')
    })
    it('writes edits of a preset shown in place of the applied one to its record, except shared protected fields', () => {
        const db = database()
        db.botPresets[1].seperateModels = { memory: 'record-b' } as Database['seperateModels']
        derive(db)
        const view = createPresetMirrorView(db, 'preset-b', apply)!
        const values = view.values
        expect(values.mainPrompt).toBe('B')
        values.mainPrompt = 'view edit'
        values.presetRegex = [{ comment: 'view regex' }]
        values.seperateModels = { memory: 'shared' }
        db.doNotChangeSeperateModels = true
        flushPresetMirrorView(db, view)
        expect(db.botPresets[1]).toMatchObject({
            mainPrompt: 'view edit', regex: [{ comment: 'view regex' }], seperateModels: { memory: 'record-b' },
        })
        expect(db.botPresets[0].mainPrompt).toBe('A')
        expect(db.mainPrompt).toBe('A')
        // A received change reaches the view without discarding its pending edit.
        values.jailbreak = 'pending'
        db.botPresets[1].autoSuggestPrefix = 'received'
        expect(derivePresetMirrorView(db, view, apply)).toBe(true)
        expect(values).toMatchObject({ autoSuggestPrefix: 'received', jailbreak: 'pending', mainPrompt: 'view edit' })
    })
    it('uses the coherent renamed and suggestion fields without maintaining opaque preset keys', () => {
        const db = database()
        derive(db)
        db.autoSuggestPrefix = 'new prefix'
        db.autoSuggestClean = true
        db.reasoningEffort = 4
        db.presetRegex = []
        db.openAIKey = 'new independent'
        flushEffectivePresetEdits(db)
        expect(db.botPresets[0]).toMatchObject({ autoSuggestPrefix: 'new prefix', autoSuggestClean: true, reasonEffort: 4, regex: [], openAIKey: 'opaque' })
    })
    it('flushes persona edits to the previous stable ID before selecting a new persona', () => {
        const db = database()
        derive(db)
        db.username = 'edited A'
        db.selectedPersona = 1
        flushEffectivePersonaEdits(db)
        deriveEffectivePersonaMirrors(db)
        expect(db.personas[0].name).toBe('edited A')
        expect(db.personas[1].name).toBe('B')
        expect(db.username).toBe('B')
    })
    it('never assigns missing IDs during load or routine derivation', () => {
        const db = database()
        delete db.botPresets[0].id
        delete db.personas[0].id
        derive(db)
        flushEffectivePresetEdits(db)
        flushEffectivePersonaEdits(db)
        expect(db.botPresets[0].id).toBeUndefined()
        expect(db.personas[0].id).toBeUndefined()
    })
})

describe('protected shared preset groups', () => {
    it.each([
        ['doNotChangeSeperateModels', 'seperateModelsForAxModels', true],
        ['doNotChangeSeperateModels', 'seperateModels', { memory: 'local' }],
        ['doNotChangeFallbackModels', 'fallbackModels', { model: ['local'] }],
        ['doNotChangeFallbackModels', 'fallbackWhenBlankResponse', true],
        ['disableSeperateParameterChangeOnPresetChange', 'seperateParameters', { overrides: { local: {} } }],
    ])('preserves %s/%s through enable, edits, switch, remote derive and disable', (flag, field, value) => {
        const db = database()
        const fields = db as unknown as Record<string, unknown>
        const first = db.botPresets[0] as unknown as Record<string, unknown>
        const second = db.botPresets[1] as unknown as Record<string, unknown>
        first[field] = 'record A'
        second[field] = 'record B'
        derive(db)
        fields[field] = value
        fields[flag] = true
        flushEffectivePresetEdits(db)
        expect(db.protectedPresetValues[field]).toEqual(value)
        expect(first[field]).toBe('record A')
        setEffectivePresetOverride(db, 'preset-b', apply)
        expect(fields[field]).toEqual(value)
        fields[field] = 'protected edit'
        flushEffectivePresetEdits(db)
        expect(db.protectedPresetValues[field]).toBe('protected edit')
        expect(second[field]).toBe('record B')
        db.protectedPresetValues[field] = 'remote protected'
        deriveEffectivePresetMirrors(db, apply)
        flushEffectivePresetEdits(db)
        expect(second[field]).toBe('record B')
        fields[flag] = false
        flushEffectivePresetEdits(db)
        expect(second[field]).toBe('remote protected')
    })
})

describe('mirror derivation without changed inputs', () => {
    function objectDatabase(): Database {
        const db = database()
        Object.assign(db.botPresets[0], { promptTemplate: [{ type: 'plain', text: 'A' }], ooba: { top_p: 1 }, seperateModels: { memory: 'record' } })
        return db
    }
    it('keeps every mirror field and the toggle variables when nothing changed', () => {
        const db = objectDatabase()
        let applied = 0
        const counted = (target: Database, preset: botPreset) => { applied++; apply(target, preset) }
        deriveEffectivePresetMirrors(db, counted)
        deriveEffectivePersonaMirrors(db)
        deriveEffectiveToggleVariables(db)
        const fields = db as unknown as Record<string, unknown>
        const keys = [...Object.keys(presetMirrorMap), 'username', 'userIcon', 'personaPrompt', 'userNote', 'globalChatVariables']
        const before = new Map(keys.map(key => [key, fields[key]]))
        for (let pass = 0; pass < 2; pass++) {
            flushEffectivePresetEdits(db)
            flushEffectivePersonaEdits(db)
            flushEffectiveToggleEdits(db)
            deriveEffectivePresetMirrors(db, counted)
            deriveEffectivePersonaMirrors(db)
            deriveEffectiveToggleVariables(db)
        }
        expect(applied).toBe(1)
        for (const key of keys) expect(fields[key], key).toBe(before.get(key))
    })
    it('re-applies after the record, the selection or an unflushed mirror changed', () => {
        const db = objectDatabase()
        derive(db)
        db.botPresets[0].promptTemplate = [{ type: 'plain', text: 'remote' }] as never
        derive(db)
        expect(db.promptTemplate).toEqual([{ type: 'plain', text: 'remote' }])
        db.botPresetsId = 1
        derive(db)
        expect(db.mainPrompt).toBe('B')
        db.mainPrompt = 'unflushed'
        derive(db)
        expect(db.mainPrompt).toBe('B')
        db.globalChatVariables = { ...db.globalChatVariables, other: 'changed' }
        const explicit = getExplicitGlobalChatVariables(db)
        explicit.other = 'remote'
        deriveEffectiveToggleVariables(db)
        expect(db.globalChatVariables.other).toBe('remote')
    })
    it('re-applies when a protection is lifted or its value changes, and keeps an unchanged protected value', () => {
        const db = objectDatabase()
        derive(db)
        db.doNotChangeSeperateModels = true
        db.seperateModels = { memory: 'protected' } as never
        flushEffectivePresetEdits(db)
        derive(db)
        const protectedValue = db.seperateModels
        derive(db)
        expect(db.seperateModels).toBe(protectedValue)
        db.protectedPresetValues.seperateModels = { memory: 'remote protected' } as never
        derive(db)
        expect(db.seperateModels).toEqual({ memory: 'remote protected' })
        db.doNotChangeSeperateModels = false
        derive(db)
        expect(db.seperateModels).toEqual({ memory: 'record' })
    })
})

describe('explicit variables and bound effective toggles', () => {
    it('opening and closing a chat derives toggles without editing explicit variables', () => {
        const db = database()
        derive(db)
        const explicit = { ...getExplicitGlobalChatVariables(db) }
        const chat = { id: 'chat', savedToggleValues: { toggle_a: 'bound', toggle_unknown: 'unknown' } } as unknown as Chat
        db.characters = [{ chaId: 'character', chats: [chat] }] as unknown as Database['characters']
        deriveEffectiveToggleVariables(db, chat)
        flushEffectiveToggleEdits(db)
        expect(db.globalChatVariables).toEqual({ toggle_a: 'bound', toggle_unknown: 'unknown', other: 'shared', extension: 'opaque' })
        expect(getExplicitGlobalChatVariables(db)).toEqual(explicit)
        deriveEffectiveToggleVariables(db)
        expect(db.globalChatVariables).toEqual(explicit)
    })
    it('flushes edits to the previous bound conversation before opening another', () => {
        const db = database()
        derive(db)
        const previous = { id: 'previous', savedToggleValues: { toggle_a: 'bound' } } as unknown as Chat
        const next = { id: 'next', savedToggleValues: { toggle_a: 'next' } } as unknown as Chat
        db.characters = [{ chaId: 'character', chats: [previous, next] }] as unknown as Database['characters']
        deriveEffectiveToggleVariables(db, previous)
        db.globalChatVariables.toggle_a = 'edited bound'
        db.globalChatVariables.other = 'edited shared'
        flushEffectiveToggleEdits(db)
        deriveEffectiveToggleVariables(db, next)
        expect(previous.savedToggleValues).toEqual({ toggle_a: 'edited bound' })
        expect(next.savedToggleValues).toEqual({ toggle_a: 'next' })
        expect(getExplicitGlobalChatVariables(db)).toMatchObject({ toggle_a: 'explicit', other: 'edited shared' })
    })
    it('keeps the toggle binding on the conversation when its object is replaced', () => {
        const db = database()
        const chat = { id: 'chat', savedToggleValues: { toggle_a: 'bound' } } as unknown as Chat
        db.characters = [{ chaId: 'character', chats: [chat] }] as unknown as Database['characters']
        derive(db)
        deriveEffectiveToggleVariables(db, chat)
        const replacement = { ...chat, savedToggleValues: { toggle_a: 'bound', toggle_b: 'received' } } as Chat
        db.characters[0].chats[0] = replacement
        db.globalChatVariables.toggle_a = 'edited'
        flushEffectiveToggleEdits(db)
        expect(replacement.savedToggleValues).toEqual({ toggle_a: 'edited', toggle_b: 'received' })
        expect(getExplicitGlobalChatVariables(db).toggle_a).toBe('explicit')
        db.characters[0].chats[0] = { ...replacement, savedToggleValues: { ...replacement.savedToggleValues } } as Chat
        expect(translateRootUnitIntents(db, [{ key: '["toggle","toggle_c"]', type: 'set', value: 'plugin' }])).toEqual([
            { key: '["conversation","character","chat","savedToggleValues"]', type: 'set',
                value: { toggle_a: 'edited', toggle_b: 'received', toggle_c: 'plugin' } },
        ])
    })
    it('captures unbound deletion and edits while retaining unknown explicit variables', () => {
        const db = database()
        derive(db)
        delete db.globalChatVariables.toggle_a
        db.globalChatVariables.toggle_added = 'added'
        flushEffectiveToggleEdits(db)
        expect(getExplicitGlobalChatVariables(db)).toEqual({ toggle_added: 'added', other: 'shared', extension: 'opaque' })
    })
})

describe('explicit upstream import identity preparation', () => {
    it('preserves root-only edits and unique IDs, replacing missing or duplicate IDs only at import', () => {
        const db = database()
        db.botPresets[1].id = 'preset-a'
        delete db.personas[1].id
        db.mainPrompt = 'upstream root'
        db.username = 'upstream name'
        db.doNotChangeFallbackModels = true
        db.fallbackWhenBlankResponse = true
        prepareImportedIdentityState(db)
        expect(db.botPresets[0]).toMatchObject({ id: 'preset-a', mainPrompt: 'upstream root', openAIKey: 'opaque' })
        expect(db.personas[0]).toMatchObject({ id: 'persona-a', name: 'upstream name' })
        expect(db.botPresets[1].id).toMatch(/^[0-9a-f-]{36}$/)
        expect(db.personas[1].id).toMatch(/^[0-9a-f-]{36}$/)
        expect(db.protectedPresetValues.fallbackWhenBlankResponse).toBe(true)
        expect(db.explicitGlobalChatVariables).toEqual(db.globalChatVariables)
        derive(db)
        expect(db.mainPrompt).toBe('upstream root')
    })
    it('does not overwrite an imported record field when the root field is absent', () => {
        const db = database()
        delete db.mainPrompt
        prepareImportedIdentityState(db)
        expect(db.botPresets[0].mainPrompt).toBe('A')
    })
})

describe('plugin root intent adapters', () => {
    it('routes mirror intents to their previous effective IDs and retains independent root values', () => {
        const db = database()
        derive(db)
        setEffectivePresetOverride(db, 'preset-b', apply)
        db.botPresetsId = 1
        expect(translateRootUnitIntents(db, [
            { key: '["root","mainPrompt"]', type: 'set', value: 'plugin edit' },
            { key: '["root","userNote"]', type: 'delete' },
            { key: '["root","openAIKey"]', type: 'set', value: 'independent' },
        ])).toEqual([
            { key: '["preset","preset-b","mainPrompt"]', type: 'set', value: 'plugin edit' },
            { key: '["persona","persona-a","note"]', type: 'delete' },
            { key: '["root","openAIKey"]', type: 'set', value: 'independent' },
        ])
    })
    it('maps index selections and protected transitions to stable units', () => {
        const db = database()
        derive(db)
        db.fallbackWhenBlankResponse = true
        const intents = translateRootUnitIntents(db, [
            { key: '["root","botPresetsId"]', type: 'set', value: 1 },
            { key: '["root","doNotChangeFallbackModels"]', type: 'set', value: true },
        ])
        expect(intents).toContainEqual({ key: '["root","botPresetsId"]', type: 'set', value: 'preset-b' })
        expect(intents).toContainEqual({ key: '["preset-protected","fallbackWhenBlankResponse"]', type: 'set', value: true })
    })
    it('routes a protected value with its enabling flag in either mutation order', () => {
        const db = database()
        derive(db)
        const value = { key: '["root","fallbackWhenBlankResponse"]', type: 'set' as const, value: true }
        const flag = { key: '["root","doNotChangeFallbackModels"]', type: 'set' as const, value: true }
        for (const batch of [[value, flag], [flag, value]]) {
            const intents = translateRootUnitIntents(db, batch)
            expect(intents.filter(intent => intent.key === '["preset-protected","fallbackWhenBlankResponse"]')).toEqual([
                { key: '["preset-protected","fallbackWhenBlankResponse"]', type: 'set', value: true },
            ])
            expect(intents.some(intent => intent.key.startsWith('["preset",'))).toBe(false)
        }
    })
    it('combines per-key plugin toggle edits into the bound conversation without shared toggle writes', () => {
        const db = database()
        const chat = { id: 'chat', savedToggleValues: { toggle_a: 'bound', toggle_delete: 'delete' } } as unknown as Chat
        db.characters = [{ chaId: 'character', chats: [chat] }] as unknown as Database['characters']
        derive(db)
        deriveEffectiveToggleVariables(db, chat)
        expect(translateRootUnitIntents(db, [
            { key: '["toggle","toggle_a"]', type: 'set', value: 'edited' },
            { key: '["toggle","toggle_delete"]', type: 'delete' },
            { key: '["variable","extension"]', type: 'set', value: 'new' },
        ])).toEqual([
            { key: '["variable","extension"]', type: 'set', value: 'new' },
            { key: '["conversation","character","chat","savedToggleValues"]', type: 'set', value: { toggle_a: 'edited' } },
        ])
        expect(getExplicitGlobalChatVariables(db).toggle_a).toBe('explicit')
        expect(chat.savedToggleValues.toggle_a).toBe('bound')
    })
    it('preserves opaque root names that also occur on Object.prototype', () => {
        const db = database()
        derive(db)
        const mutation = { key: '["root","toString"]', type: 'set' as const, value: 'opaque' }
        expect(translateRootUnitIntents(db, [mutation])).toEqual([mutation])
    })
    it('deletes explicit variable units when a whole variable map is removed', () => {
        const db = database()
        derive(db)
        expect(translateRootUnitIntents(db, [{ key: '["root","globalChatVariables"]', type: 'delete' }])).toEqual([
            { key: '["toggle","toggle_a"]', type: 'delete' },
            { key: '["variable","other"]', type: 'delete' },
            { key: '["variable","extension"]', type: 'delete' },
        ])
    })
})
