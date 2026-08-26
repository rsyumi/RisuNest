import { describe, expect, it } from 'vitest'
import { languageChinese } from '../../lang/cn'
import { languageGerman } from '../../lang/de'
import { languageEnglish } from '../../lang/en'
import { languageSpanish } from '../../lang/es'
import { languageKorean } from '../../lang/ko'
import { languageVietnamese } from '../../lang/vi'
import { languageChineseTraditional } from '../../lang/zh-Hant'
import { advancedSettingsItems } from './advancedSettingsData'

describe('streaming display optimization setting', () => {
    it('is visible without enabling experimental settings', () => {
        const setting = advancedSettingsItems.find((item) => item.id === 'adv.streamingDisplayOpt')

        expect(setting).toBeDefined()
        expect(setting?.condition).toBeUndefined()
        expect(setting?.showExperimental).toBeUndefined()
    })

    it('describes provider updates instead of model tokens in all maintained locales', () => {
        const languages = [
            languageEnglish,
            languageKorean,
            languageChinese,
            languageChineseTraditional,
            languageVietnamese,
            languageGerman,
            languageSpanish,
        ]

        for (const language of languages) {
            const description = language.help?.streamingDisplayOptimizationMode
            expect(description).toBeTypeOf('string')
            expect(description).not.toMatch(/token|토큰/i)
            expect(description).toMatch(/Lua/)
        }
    })
})
