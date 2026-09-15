import { describe, expect, it } from 'vitest'
import { parseReleaseNotes, selectLocalizedNotes } from './notes'

describe('release notes', () => {
    it('selects locale, English, first locale, then default notes', () => {
        const notes = { en: 'English', ko: '한국어' }
        expect(selectLocalizedNotes(notes, 'ko-KR', 'Default')).toBe('한국어')
        expect(selectLocalizedNotes(notes, 'de', 'Default')).toBe('English')
        expect(selectLocalizedNotes({ fr: 'Français' }, 'de', 'Default')).toBe('Français')
        expect(selectLocalizedNotes({}, 'de', 'Default')).toBe('Default')
    })

    it('parses only safe display tokens and never emits HTML', () => {
        const blocks = parseReleaseNotes('## Changes\n\n- **Fast** `code` [safe](https://example.com) [bad](javascript:alert(1))\n<script>run()</script>')
        expect(JSON.stringify(blocks)).not.toContain('<script>')
        expect(JSON.stringify(blocks)).toContain('https://example.com')
        expect(JSON.stringify(blocks)).not.toContain('javascript:')
    })

    it('truncates oversized localized notes at a line boundary', () => {
        const value = `${'a'.repeat(3000)}\n${'b'.repeat(3000)}`
        expect(selectLocalizedNotes({ en: value }, 'en', '')).toBe('a'.repeat(3000))
    })
})
