import { describe, expect, it } from 'vitest'
import { hasNonDefaultBindingData, validateBindingCount, type BindingLocalContent } from './bindingDefaults'

function defaults(): BindingLocalContent {
    const library = { characters: [{ chaId: 'fresh-id', name: 'Factory', chats: [{ id: 'chat-id', message: [] }], chatPage: 0 }], botPresets: [{ id: 'preset-id', name: 'Default' }], personas: [{ id: 'persona-id', name: 'User' }], jailbreakToggle: false }
    return { library: structuredClone(library), characterCount: '0', factoryLibrary: library, opaqueSharedUnitCount: '0', managedAliasCount: '0', factoryManagedAliasCount: '0', ordinaryPluginValueCount: '0', hypaValueCount: '0', pluginLocalValueCount: '0' }
}

it('preserves exact canonical decimal count strings beyond Number precision', () => {
    for (const value of ['0', '9007199254740993', '18446744073709551615']) {
        expect(validateBindingCount(value)).toBe(value)
        expect(typeof validateBindingCount(value)).toBe('string')
    }
})
it.each([0, null, undefined, '', '00', '01', '+1', '-1', '1.0', '1e3', ' 1', '18446744073709551616'])('rejects a noncanonical count %s', value => {
    expect(() => validateBindingCount(value)).toThrow('Invalid binding content count')
})

describe('first binding non-default data', () => {
    it('accepts current factory defaults', () => expect(hasNonDefaultBindingData(defaults())).toBe(false))
    it('ignores generated record IDs and selection indexes', () => {
        const data = defaults()
        ;(data.factoryLibrary.characters as any[])[0].chats[0].promptPresetId = 'preset-id'
        Object.assign((data.library.characters as any[])[0], { chaId: 'other', chatPage: 3 })
        Object.assign((data.library.characters as any[])[0].chats[0], { id: 'other-chat', promptPresetId: 'other-preset' })
        ;(data.library.botPresets as any[])[0].id = 'other-preset'
        ;(data.library.personas as any[])[0].id = 'other-persona'
        data.library.botPresetsId = 99
        data.library.selectedPersona = 99
        expect(hasNonDefaultBindingData(data)).toBe(false)
    })
    it.each(['edited', 'added', 'deleted'])('counts %s records', action => {
        const data = defaults()
        const records = data.library.characters as any[]
        if (action === 'edited') records[0].name = 'User edit'
        if (action === 'added') records.push({ chaId: 'custom', name: 'Custom', chats: [] })
        if (action === 'deleted') records.pop()
        expect(hasNonDefaultBindingData(data)).toBe(true)
    })
    it('counts shared settings and unknown shared content', () => {
        const data = defaults()
        data.library.jailbreakToggle = true
        expect(hasNonDefaultBindingData(data)).toBe(true)
        data.library.jailbreakToggle = false
        data.library.upstreamExtension = 'user value'
        expect(hasNonDefaultBindingData(data)).toBe(true)
    })
    it('counts any character the native store reports', () => {
        const data = defaults()
        data.library.characters = []
        data.factoryLibrary.characters = []
        expect(hasNonDefaultBindingData(data)).toBe(false)
        data.characterCount = '1'
        expect(hasNonDefaultBindingData(data)).toBe(true)
    })
    it.each(['ordinaryPluginValueCount', 'hypaValueCount', 'pluginLocalValueCount', 'managedAliasCount'] as const)('counts %s independently of participation', key => {
        const data = defaults(); data[key] = '1'
        expect(hasNonDefaultBindingData(data)).toBe(true)
    })
    it('ignores other device-local settings, credentials and derived mirrors', () => {
        const data = defaults()
        Object.assign(data.library, { account: { token: 'synthetic' }, statics: { messages: 100 }, mainPrompt: 'derived', selectedPersona: 4 })
        expect(hasNonDefaultBindingData(data)).toBe(false)
    })
    it('does not ignore ordinary nested values named id', () => {
        const data = defaults()
        data.sharedVariables = { id: 'user value' }
        expect(hasNonDefaultBindingData(data)).toBe(true)
    })
})

it('counts opaque stored shared units without a factory counterpart', () => {
    const data = defaults(); data.opaqueSharedUnitCount = '1'
    expect(hasNonDefaultBindingData(data)).toBe(true)
})
it('does not normalize arbitrary content just because it matches a generated ID', () => {
    const data = defaults()
    data.factoryLibrary.jailbreakToggle = 'fresh-id'
    data.library.jailbreakToggle = 'other'
    ;(data.library.characters as any[])[0].chaId = 'other'
    expect(hasNonDefaultBindingData(data)).toBe(true)
})

it('counts modified protected preset settings and ignores identical protected defaults', () => {
    const data = defaults(); data.factoryLibrary.seperateModels = { model: 'factory' }
    data.protectedValues = { seperateModels: { model: 'factory' } }
    expect(hasNonDefaultBindingData(data)).toBe(false)
    data.protectedValues = { seperateModels: { model: 'custom' } }
    expect(hasNonDefaultBindingData(data)).toBe(true)
})

it('native backing views do not make default data non-default', () => {
    const data = defaults(); Object.assign(data.library,{explicitGlobalChatVariables:{},protectedPresetValues:{}})
    expect(hasNonDefaultBindingData(data)).toBe(false)
})
