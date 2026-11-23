import { describe, expect, it } from 'vitest'
import { CHARACTER_UPDATE, applyUpdate, planUpdate, type UpdateItem } from './merge'
import { editStartText, editableText, editedChoice, lineDiff, updateItemName, updateSearchText, updateSideValue, updateValueText } from './view'

const item = (field: string, mine: unknown, theirs: unknown) => ({ id: field, field, key: 'k', mine, theirs, base: undefined, baseKnown: false, status: 'differs', choice: null }) as UpdateItem

describe('update items', () => {
    it('starts an edit from no text when a side holds no text of that kind', () => {
        expect(editableText(item('desc', 'mine', { kept: true }), 'theirs')).toBe('')
        expect(editableText(item('globalLore', { content: 'lore' }, { content: ['not text'] }), 'theirs')).toBe('')
        expect(editableText(item('globalLore', { content: 'lore' }, { content: 'new' }), 'theirs')).toBe('new')
    })

    it('writes an edit into what 내 버전 writes, keeping the fields only the author changed', () => {
        const entry = (content: string, more = {}) => ({ key: 'town', secondkey: '', comment: 'Town', mode: 'normal', insertorder: 100, alwaysActive: false, selective: false, content, ...more })
        const compare = (mine: object, theirs: object) => {
            const plan = planUpdate(CHARACTER_UPDATE, { globalLore: [mine] } as any, { globalLore: [theirs] } as any, { globalLore: [entry('town')] } as any)
            const item = plan.items[0]
            const write = (text: string) => (applyUpdate(CHARACTER_UPDATE, { globalLore: [mine] } as any, { globalLore: [theirs] } as any, plan, { [item.id]: editedChoice(item, text) }) as any).globalLore[0]
            return { item, write }
        }
        // Both changed the insert order, which the text box does not cover; only the user changed the text.
        const conflict = compare(entry('마을', { insertorder: 5 }), entry('town', { insertorder: 7, alwaysActive: true }))
        expect(conflict.item.status).toBe('conflict')
        expect(updateSideValue(conflict.item, 'theirs')).toMatchObject({ content: '마을', insertorder: 7, alwaysActive: true })
        expect([editStartText(conflict.item), editableText(conflict.item, 'theirs')]).toEqual(['마을', '마을'])
        expect(conflict.write('written')).toMatchObject({ content: 'written', insertorder: 5, alwaysActive: true })
        // A merged item starts from the merged entry, and each side from that whole side.
        const merged = compare(entry('town', { insertorder: 5 }), entry('town!'))
        expect(merged.item.status).toBe('merged')
        expect([editStartText(merged.item), editableText(merged.item, 'mine')]).toEqual(['town!', 'town'])
        expect(merged.write('x')).toMatchObject({ content: 'x', insertorder: 5 })
        // A text field is the text itself.
        const desc = planUpdate(CHARACTER_UPDATE, { desc: 'a' } as any, { desc: 'b' } as any, null)
        expect(applyUpdate(CHARACTER_UPDATE, { desc: 'a' } as any, { desc: 'b' } as any, desc, { desc: editedChoice(desc.items[0], 'c') })).toEqual({ desc: 'c' })
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
