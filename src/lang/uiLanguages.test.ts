import { expect, it } from 'vitest'
import { uiLanguageFor } from './uiLanguages'

it('picks the offered UI language for a device language tag and falls back to English', () => {
    expect(uiLanguageFor('ko-KR')).toBe('ko')
    expect(uiLanguageFor('ko')).toBe('ko')
    expect(uiLanguageFor('en-GB')).toBe('en')
    expect(uiLanguageFor('de-DE')).toBe('en')
    expect(uiLanguageFor('zh-TW')).toBe('en')
})
