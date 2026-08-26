import { describe, expect, it } from 'vitest'
import {
    createChatScreenshotDialogSnapshot,
    createChatScreenshotJob,
    createChatScreenshotJobFromDialogSnapshot,
    fullScreenshotRange,
    recentScreenshotRange,
    validateScreenshotRange,
} from './chatScreenshotRange'

function parserContext() {
    const character = {
        type: 'character' as const,
        name: 'Character',
        chaId: 'character-1',
        chatPage: 0,
        chats: [{ message: [], note: '', name: '', localLore: [] }],
        customscript: [],
    }
    return {
        database: { characters: [character] } as any,
        character: character as any,
        userName: 'User',
        personaPrompt: '',
        modules: [],
        moduleLorebooks: [],
        selectedCharID: 0,
        chatVariables: {},
        globalChatVariables: {},
        currentTime: 1,
    }
}

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
                parserContext: parserContext(),
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
                parserContext: parserContext(),
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

    it('keeps dialog-open identity and messages after the live source changes', () => {
        const messages = [
            { role: 'user' as const, data: 'open-time first' },
            { role: 'char' as const, data: 'open-time second' },
        ]
        const context = parserContext()
        context.userName = 'Open User'
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: 'open-character',
            chatId: 'open-chat',
            messages,
            renderContext: {
                character: null,
                characterName: 'Open Character',
                characterImageSource: '',
                characterLargePortrait: false,
                userName: 'Open User',
                userImageSource: '',
                userLargePortrait: false,
                moduleAssets: [],
                presetRegex: [],
                moduleRegexScripts: [],
                assetStyle: '',
                parserContext: context,
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

        messages[0].data = 'changed first'
        messages.push({ role: 'char', data: 'changed third' })
        context.userName = 'Changed User'

        const job = createChatScreenshotJobFromDialogSnapshot(dialogSnapshot, 1, 2)

        expect(job).toMatchObject({
            characterId: 'open-character',
            chatId: 'open-chat',
            totalTurns: 2,
        })
        expect(job.messages.map((message) => message.data)).toEqual([
            'open-time first',
            'open-time second',
        ])
        expect(job.renderContext.parserContext.userName).toBe('Open User')
        expect(Object.isFrozen(dialogSnapshot.messages[0])).toBe(true)
    })

    it('keeps the selected messages and the derived frozen history window needed by CBS', () => {
        const messages = [
            { role: 'char' as const, data: 'too old' },
            { role: 'user' as const, data: 'previous' },
            { role: 'char' as const, data: 'selected one' },
            { role: 'user' as const, data: 'selected two' },
        ]
        const context = parserContext()
        const job = createChatScreenshotJob({
            characterId: 'character-1',
            chatId: 'chat-1',
            messages,
            start: 3,
            end: 4,
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
                parserContext: context,
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

        const parserMessages = job.renderContext.parserContext.character.chats[0].message
        expect(parserMessages.map((message) => message.data)).toEqual([
            'too old',
            'previous',
            'selected one',
            'selected two',
        ])
        expect(parserMessages[2]).toBe(job.messages[0])
        expect(job.renderContext.historyStartIndex).toBe(0)
        expect(job.renderContext.firstParserMessageIndex).toBe(2)
    })

    it('keeps the parser history projection bounded when full history is not requested', () => {
        const messages = Array.from({ length: 100 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' as const : 'user' as const,
            data: `turn ${index + 1}`,
        }))
        const job = createChatScreenshotJob({
            characterId: 'character-1',
            chatId: 'chat-1',
            messages,
            start: 100,
            end: 100,
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
                parserContext: parserContext(),
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

        expect(job.renderContext.historyStartIndex).toBe(95)
        expect(job.renderContext.parserContext.character.chats[0].message).toHaveLength(5)
        expect(job.messages).toHaveLength(1)
    })
})
