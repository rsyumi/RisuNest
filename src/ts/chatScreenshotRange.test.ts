import { describe, expect, it } from 'vitest'
import {
    createChatScreenshotJob,
    fullScreenshotRange,
    recentScreenshotRange,
    validateScreenshotRange,
} from './chatScreenshotRange'

describe('chat screenshot ranges', () => {
    it('accepts 1-based inclusive turn bounds and rejects invalid input', () => {
        expect(validateScreenshotRange(5, 1, 5)).toEqual({ ok: true, start: 1, end: 5 })
        expect(validateScreenshotRange(5, 1.5, 3)).toMatchObject({ ok: false, reason: 'integer' })
        expect(validateScreenshotRange(5, 0, 3)).toMatchObject({ ok: false, reason: 'bounds' })
        expect(validateScreenshotRange(5, 4, 3)).toMatchObject({ ok: false, reason: 'order' })
        expect(validateScreenshotRange(0, 1, 1)).toMatchObject({ ok: false, reason: 'empty' })
    })

    it('selects Recent 50 and Full ranges', () => {
        expect(recentScreenshotRange(120)).toEqual({ start: 71, end: 120 })
        expect(recentScreenshotRange(20)).toEqual({ start: 1, end: 20 })
        expect(fullScreenshotRange(120)).toEqual({ start: 1, end: 120 })
    })

    it('creates a deep immutable snapshot of the selected conversation', () => {
        const messages = [
            { role: 'user' as const, data: 'one', generationInfo: { model: 'a' } },
            { role: 'char' as const, data: 'two' },
            { role: 'user' as const, data: 'three' },
        ]

        const job = createChatScreenshotJob({
            characterId: 'character-1',
            chatId: 'chat-1',
            messages,
            start: 1,
            end: 2,
            renderContext: {
                character: null,
                characterName: 'Character',
                characterImageSource: 'character.png',
                characterLargePortrait: false,
                userName: 'User',
                userImageSource: 'user.png',
                userLargePortrait: false,
                moduleAssets: [['Module asset', 'module.png', 'png']],
                presetRegex: [],
                moduleRegexScripts: [],
                assetStyle: 'default',
                settings: {
                    autoTranslate: false,
                    autoTranslateCachedOnly: false,
                    translatorType: 'google',
                    translateBeforeHTMLFormatting: false,
                    legacyTranslation: false,
                    showTranslationLoading: false,
                    newImageHandlingBeta: true,
                    assetWidth: 12,
                    hideAllImages: false,
                    iconSize: 100,
                    zoomSize: 100,
                    lineHeight: 1.25,
                    dynamicAssets: false,
                    dynamicAssetsEditDisplay: false,
                    legacyMediaFindings: false,
                    assetMaxDifference: 0.5,
                },
            },
        })

        messages[0].data = 'edited'
        messages[0].generationInfo!.model = 'b'
        messages.push({ role: 'char', data: 'appended' })

        expect(job).toMatchObject({
            characterId: 'character-1',
            chatId: 'chat-1',
            start: 1,
            end: 2,
            totalTurns: 3,
        })
        expect(job.messages).toEqual([
            { role: 'user', data: 'one', generationInfo: { model: 'a' } },
            { role: 'char', data: 'two' },
        ])
        expect(Object.isFrozen(job)).toBe(true)
        expect(Object.isFrozen(job.messages[0].generationInfo)).toBe(true)
        expect(Object.isFrozen(job.renderContext.moduleAssets)).toBe(true)
    })

    it('snapshots live proxy-backed messages', () => {
        const message = new Proxy(
            { role: 'user' as const, data: 'proxied', generationInfo: { model: 'model' } },
            {},
        )

        const job = createChatScreenshotJob({
            characterId: 'character',
            chatId: 'chat',
            messages: [message],
            start: 1,
            end: 1,
            renderContext: {
                character: null,
                characterName: 'Character',
                characterImageSource: '',
                characterLargePortrait: false,
                userName: 'User',
                userImageSource: '',
                userLargePortrait: false,
                moduleAssets: [],
                presetRegex: [],
                moduleRegexScripts: [],
                assetStyle: '',
                settings: {
                    autoTranslate: false,
                    autoTranslateCachedOnly: false,
                    translatorType: 'google',
                    translateBeforeHTMLFormatting: false,
                    legacyTranslation: false,
                    showTranslationLoading: false,
                    newImageHandlingBeta: false,
                    assetWidth: -1,
                    hideAllImages: false,
                    iconSize: 100,
                    zoomSize: 100,
                    lineHeight: 1.25,
                    dynamicAssets: false,
                    dynamicAssetsEditDisplay: false,
                    legacyMediaFindings: false,
                    assetMaxDifference: 0.5,
                },
            },
        })

        expect(job.messages).toEqual([
            { role: 'user', data: 'proxied', generationInfo: { model: 'model' } },
        ])
    })
})
