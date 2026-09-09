import { expect, it } from 'vitest'
import {
    applyToggleValues,
    countToggleChanges,
    defaultChatToggleBinding,
    snapshotToggleValues,
} from './toggleBindings'

it('pins global toggles, restores orphan keys and resets only current definitions', () => {
    const values = { toggle_a: '1', toggle_old: 'keep', unrelated: 'keep', jailbreakToggle: '1' }
    applyToggleValues(values, { toggle_orphan: '0', toggle_empty: '' }, ['toggle_a', 'jailbreakToggle'])
    expect(values).toEqual({
        toggle_old: 'keep',
        unrelated: 'keep',
        jailbreakToggle: '1',
        toggle_orphan: '0',
        toggle_empty: '',
    })
    expect(snapshotToggleValues(values)).toEqual({ toggle_old: 'keep', toggle_orphan: '0', toggle_empty: '' })
    expect(countToggleChanges(values, { toggle_old: 'keep', toggle_orphan: '1' })).toBe(1)
})

it('distinguishes an unbound chat from a pinned empty snapshot and clones defaults', () => {
    expect(defaultChatToggleBinding({})).toEqual({})
    expect(defaultChatToggleBinding({ defaultToggleValues: {} })).toEqual({ savedToggleValues: {} })
    const defaults = { toggle_a: '1' }
    const chat = defaultChatToggleBinding({ defaultToggleValues: defaults })
    defaults.toggle_a = '0'
    expect(chat.savedToggleValues).toEqual({ toggle_a: '1' })
    expect(defaultChatToggleBinding({ defaultToggleValues: {}, disableToggleBinding: true })).toEqual({})
})
