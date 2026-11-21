import { describe, expect, it } from 'vitest'
import type { UpdateItem } from './merge'
import { editableText, lineDiff, updateItemName, updateSearchText, updateValueText } from './view'

const item = (field: string, mine: unknown, theirs: unknown) => ({ id: field, field, key: 'k', mine, theirs, base: undefined, baseKnown: false, status: 'differs', choice: null }) as UpdateItem

describe('update items', () => {
    it('starts an edit from no text when a side holds no text of that kind', () => {
        expect(editableText(item('desc', 'mine', { kept: true }), 'theirs')).toBe('')
        expect(editableText(item('globalLore', { content: 'lore' }, { content: ['not text'] }), 'theirs')).toBe('')
        expect(editableText(item('globalLore', { content: 'lore' }, { content: 'new' }), 'theirs')).toBe('new')
    })

    it('names a regex without a comment by its pattern and finds entries by their keys and pattern', () => {
        const regex = item('customscript', { in: 'pattern-a', out: 'x' }, { in: 'pattern-a', out: 'y' })
        expect(updateItemName(regex)).toBe('pattern-a')
        const lore = item('globalLore', { key: 'dragon', secondkey: 'wyrm', content: 'body' }, { key: 'dragon', secondkey: 'wyrm', content: 'new body' })
        expect(updateSearchText(lore, 'Lorebook')).toContain('wyrm')
        expect(updateSearchText({ ...regex, mine: { comment: 'Named', in: 'pattern-b', out: 'x' } }, 'Regex')).toContain('pattern-b')
    })
})

describe('updateValueText', () => {
    it('shows the code of a trigger effect line by line, so one changed line diffs as one', () => {
        const trigger = (code: string) => ({ comment: 'lua', type: 'start', conditions: [], effect: [{ type: 'triggerlua', code }], lowLevelAccess: false })
        const diff = lineDiff(updateValueText(trigger('a = 1\nb = 2\nc = 3')), updateValueText(trigger('a = 1\nb = 5\nc = 3')))
        expect(diff.filter((line) => line.type === 'add' || line.type === 'del')).toEqual([
            { type: 'del', text: 'b = 2' },
            { type: 'add', text: 'b = 5' },
        ])
    })
})
