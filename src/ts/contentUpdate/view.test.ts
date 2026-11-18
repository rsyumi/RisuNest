import { describe, expect, it } from 'vitest'
import { lineDiff, updateValueText } from './view'

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
