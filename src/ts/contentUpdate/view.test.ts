import { describe, expect, it } from 'vitest'
import { CHARACTER_UPDATE, planUpdate, type UpdateItem } from './merge'
import { editStartText, editableText, editedValue, lineDiff, updateItemName, updateSearchText, updateSideValue, updateValueText } from './view'

const item = (field: string, mine: unknown, theirs: unknown) => ({ id: field, field, key: 'k', mine, theirs, base: undefined, baseKnown: false, status: 'differs', choice: null }) as UpdateItem

describe('update items', () => {
    it('starts an edit from no text when a side holds no text of that kind', () => {
        expect(editableText(item('desc', 'mine', { kept: true }), 'theirs')).toBe('')
        expect(editableText(item('globalLore', { content: 'lore' }, { content: ['not text'] }), 'theirs')).toBe('')
        expect(editableText(item('globalLore', { content: 'lore' }, { content: 'new' }), 'theirs')).toBe('new')
    })

    it('writes an edit into what 내 버전 writes, keeping the fields only the author changed', () => {
        const entry = (content: string, more = {}) => ({ key: 'town', secondkey: '', comment: 'Town', mode: 'normal', insertorder: 100, alwaysActive: false, selective: false, content, ...more })
        const plan = (mine: object, theirs: object) => planUpdate(CHARACTER_UPDATE, { globalLore: [mine] } as any, { globalLore: [theirs] } as any, { globalLore: [entry('town')] } as any).items[0]
        // Both changed the insert order, which the text box does not cover; only the user changed the text.
        const conflict = plan(entry('마을', { insertorder: 5 }), entry('town', { insertorder: 7, alwaysActive: true }))
        expect(conflict.status).toBe('conflict')
        expect(updateSideValue(conflict, 'theirs')).toMatchObject({ content: '마을', insertorder: 7, alwaysActive: true })
        expect([editStartText(conflict), editableText(conflict, 'theirs')]).toEqual(['마을', '마을'])
        expect(editedValue(conflict, 'written')).toMatchObject({ content: 'written', insertorder: 5, alwaysActive: true })
        // A merged item starts from the merged entry, and each side from that whole side.
        const merged = plan(entry('town', { insertorder: 5 }), entry('town!'))
        expect(merged.status).toBe('merged')
        expect([editStartText(merged), editableText(merged, 'mine')]).toEqual(['town!', 'town'])
        expect(editedValue(merged, 'x')).toMatchObject({ content: 'x', insertorder: 5 })
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
