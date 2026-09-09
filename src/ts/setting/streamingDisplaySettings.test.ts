import { describe, expect, it } from 'vitest'
import { languageChinese } from '../../lang/cn'
import { languageGerman } from '../../lang/de'
import { languageEnglish } from '../../lang/en'
import { languageSpanish } from '../../lang/es'
import { languageKorean } from '../../lang/ko'
import { languageVietnamese } from '../../lang/vi'
import { languageChineseTraditional } from '../../lang/zh-Hant'
import { advancedSettingsItems } from './advancedSettingsData'
import { risuNestSettingsItems } from './risuNestSettingsData'

describe('streaming display optimization setting', () => {
    it('offers independent thought and effect controls only in RisuNest settings', () => {
        const thought = risuNestSettingsItems.find(
            (item) => item.id === 'risunest.streaming.thoughtMode',
        )
        const effects = risuNestSettingsItems.find(
            (item) => item.id === 'risunest.streaming.deferEffects',
        )
        expect(thought).toMatchObject({
            type: 'segmented',
            bindKey: 'streamingThoughtMode',
        })
        expect(
            thought?.options?.segmentOptions?.map((option) => option.value),
        ).toEqual(['recent', 'collapsed', 'off'])
        expect(effects).toMatchObject({
            type: 'check',
            bindKey: 'streamingDeferDisplayProcessing',
        })
        for (const item of [thought, effects]) {
            expect(item?.condition).toBeUndefined()
            expect(item?.showExperimental).toBeUndefined()
        }
        expect(
            advancedSettingsItems.some((item) =>
                [
                    'streamingThoughtMode',
                    'streamingDeferDisplayProcessing',
                    'streamingDisplayOptimizationMode',
                ].includes(item.bindKey ?? ''),
            ),
        ).toBe(false)
        for (const locale of [languageEnglish, languageKorean]) {
            expect(locale.risuNest.streaming.thoughtModeHelp).toMatch(/Lua/)
            expect(locale.risuNest.streaming.deferEffectsHelp).toMatch(/Lua/)
        }
    })
    it('does not expose legacy monotonically growing chat page controls', () => {
        const ids = advancedSettingsItems.map((item) => item.id)
        expect(ids).not.toContain('adv.chatLoadInitial')
        expect(ids).not.toContain('adv.chatLoadAdditional')
    })

    it('is visible without enabling experimental settings', () => {
        const setting = risuNestSettingsItems.find(
            (item) => item.id === 'risunest.streaming.outputProcessing',
        )

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
