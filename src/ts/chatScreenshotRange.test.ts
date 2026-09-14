import { Buffer } from 'buffer'
import { describe, expect, it } from 'vitest'
import {
    createChatScreenshotDialogSnapshot,
    createChatScreenshotJobFromDialogSnapshot,
    fullScreenshotRange,
    recentScreenshotRange,
    validateScreenshotRange,
    type ChatScreenshotRangeReader,
    type ChatScreenshotRenderContext,
} from './chatScreenshotRange'
import type { Message, character } from './storage/database.svelte'

function rangeReader(messages: Message[]) {
    const frozen = structuredClone(messages)
    const reads: Array<{ startIndex: number; limit: number }> = []
    const reader: ChatScreenshotRangeReader = {
        characterId: 'open-character',
        chatId: 'open-chat',
        revision: 7,
        totalTurns: frozen.length,
        async readRange(startIndex, limit) {
            reads.push({ startIndex, limit })
            return structuredClone(frozen.slice(startIndex, startIndex + limit))
        },
    }
    return { reader, reads }
}

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

function renderContext(): ChatScreenshotRenderContext {
    return {
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
    }
}

async function createProductScreenshotJob(
    messages: Message[],
    start: number,
    end: number,
    context = renderContext(),
) {
    const { reader } = rangeReader(messages)
    const snapshot = createChatScreenshotDialogSnapshot({
        characterId: reader.characterId,
        chatId: reader.chatId,
        revision: reader.revision,
        sessionVersion: 1,
        totalTurns: reader.totalTurns,
        renderContext: context,
    })
    return createChatScreenshotJobFromDialogSnapshot(snapshot, reader, start, end)
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

    it('creates a deep immutable snapshot of the selected conversation', async () => {
        const messages = [
            { role: 'user' as const, data: 'one', generationInfo: { model: 'a' } },
            { role: 'char' as const, data: 'two' },
            { role: 'user' as const, data: 'three' },
        ]

        const context = renderContext()
        context.characterImageSource = 'character.png'
        context.userImageSource = 'user.png'
        context.moduleAssets = [['Module asset', 'module.png', 'png']]
        context.assetStyle = 'default'
        context.settings.newImageHandlingBeta = true
        context.settings.assetWidth = 12
        const job = await createProductScreenshotJob(messages, 1, 2, context)

        messages[0].data = 'edited'
        messages[0].generationInfo!.model = 'b'
        messages.push({ role: 'char', data: 'appended' })

        expect(job).toMatchObject({
            characterId: 'open-character',
            chatId: 'open-chat',
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

    it('keeps only dialog-open identity, revision, count, and render context', async () => {
        const messages = [
            { role: 'user' as const, data: 'open-time first' },
            { role: 'char' as const, data: 'open-time second' },
        ]
        const { reader } = rangeReader(messages)
        const context = parserContext()
        context.userName = 'Open User'
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: 'open-character',
            chatId: 'open-chat',
            revision: 7,
            sessionVersion: 3,
            totalTurns: messages.length,
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

        const job = await createChatScreenshotJobFromDialogSnapshot(
            dialogSnapshot,
            reader,
            1,
            2,
        )

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
        expect(dialogSnapshot).toMatchObject({ revision: 7, sessionVersion: 3 })
        expect('messages' in dialogSnapshot).toBe(false)
    })

    it('pages only the selected range and exact bounded parser prefix', async () => {
        const messages = Array.from({ length: 100 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' as const : 'user' as const,
            data: `turn ${index + 1}`,
        }))
        const { reader, reads } = rangeReader(messages)
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: reader.characterId,
            chatId: reader.chatId,
            revision: reader.revision,
            sessionVersion: 1,
            totalTurns: reader.totalTurns,
            renderContext: renderContext(),
        })

        const job = await createChatScreenshotJobFromDialogSnapshot(
            dialogSnapshot,
            reader,
            100,
            100,
        )

        expect(job.renderContext.historyStartIndex).toBe(95)
        expect(job.renderContext.parserContext.character.chats[0].message).toHaveLength(5)
        expect(job.messages).toHaveLength(1)
        expect(reads[0]).toEqual({ startIndex: 99, limit: 1 })
        expect(reads).not.toContainEqual({ startIndex: 0, limit: 100 })
        expect(reads.at(-1)).toEqual({ startIndex: 95, limit: 4 })
    })

    it('pages the full pinned history only when parser semantics require it', async () => {
        const messages = Array.from({ length: 5_000 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' as const : 'user' as const,
            data: index === 2_499 ? '{{lastmessage}}' : `turn ${index + 1}`,
        }))
        const { reader, reads } = rangeReader(messages)
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: reader.characterId,
            chatId: reader.chatId,
            revision: reader.revision,
            sessionVersion: 1,
            totalTurns: reader.totalTurns,
            renderContext: renderContext(),
        })

        const job = await createChatScreenshotJobFromDialogSnapshot(
            dialogSnapshot,
            reader,
            2_500,
            2_500,
        )

        expect(job.renderContext.historyStartIndex).toBe(0)
        expect(job.renderContext.parserContext.character.chats[0].message).toHaveLength(5_000)
        expect(reads[0]).toEqual({ startIndex: 2_499, limit: 1 })
        expect(reads.slice(1)).toEqual([
            { startIndex: 0, limit: 2_499 },
            { startIndex: 2_500, limit: 2_500 },
        ])
    })

    it.each([
        '{{message_unixtime_array}}',
        '{{pick::one::two}}',
        '{{rollp::1d6}}',
        '{{rollpick::1d6}}',
    ])('keeps the full pinned message-count semantics for %s', async (macro) => {
        const messages = Array.from({ length: 100 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' as const : 'user' as const,
            data: index === 99 ? macro : `turn ${index + 1}`,
        }))
        const { reader } = rangeReader(messages)
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: reader.characterId,
            chatId: reader.chatId,
            revision: reader.revision,
            sessionVersion: 1,
            totalTurns: reader.totalTurns,
            renderContext: renderContext(),
        })

        const job = await createChatScreenshotJobFromDialogSnapshot(
            dialogSnapshot,
            reader,
            100,
            100,
        )

        expect(job.renderContext.historyStartIndex).toBe(0)
        expect(job.renderContext.parserContext.character.chats[0].message).toHaveLength(100)
    })

    it.each([
        ['normalized alias', '{{user-history}}'],
        ['dynamic previouschatlog', '{{previouschatlog::{{getvar::target}}}}'],
        [
            'decoded risu-style CBS',
            `<risu-style>${Buffer.from('.turn{content:"{{history}}"}').toString('hex')}</risu-style>`,
        ],
        ['ambiguous risu-style', '<risu-style>not-hex</risu-style>'],
    ])('falls back to full pinned history for %s', async (_caseName, parserInput) => {
        const messages = Array.from({ length: 100 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' as const : 'user' as const,
            data: index === 99 ? parserInput : `turn ${index + 1}`,
        }))
        const { reader } = rangeReader(messages)
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: reader.characterId,
            chatId: reader.chatId,
            revision: reader.revision,
            sessionVersion: 1,
            totalTurns: reader.totalTurns,
            renderContext: renderContext(),
        })

        const job = await createChatScreenshotJobFromDialogSnapshot(
            dialogSnapshot,
            reader,
            100,
            100,
        )

        expect(job.renderContext.historyStartIndex).toBe(0)
        expect(job.renderContext.parserContext.character.chats[0].message).toHaveLength(100)
    })

    it('extends the pinned projection for previouschatlog without rereading the selection', async () => {
        const messages = Array.from({ length: 100 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' as const : 'user' as const,
            data: index === 89 ? '{{previouschatlog::10}}' : `turn ${index + 1}`,
        }))
        const { reader, reads } = rangeReader(messages)
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: reader.characterId,
            chatId: reader.chatId,
            revision: reader.revision,
            sessionVersion: 1,
            totalTurns: reader.totalTurns,
            renderContext: renderContext(),
        })

        const job = await createChatScreenshotJobFromDialogSnapshot(
            dialogSnapshot,
            reader,
            90,
            90,
        )

        expect(job.renderContext.historyStartIndex).toBe(10)
        expect(job.renderContext.firstParserMessageIndex).toBe(79)
        expect(job.renderContext.parserContext.character.chats[0].message).toHaveLength(80)
        expect(reads[0]).toEqual({ startIndex: 89, limit: 1 })
        expect(reads.at(-1)).toEqual({ startIndex: 10, limit: 79 })
    })

    it('hydrates only speaker characters referenced by the pinned parser projection', async () => {
        const { reader } = rangeReader([
            { role: 'char', data: 'member response', saying: 'member-1' },
        ])
        const member = {
            type: 'character',
            chaId: 'member-1',
            name: 'Member',
            chats: [],
            chatPage: 0,
        } as character
        reader.readCharacter = async (characterId) =>
            characterId === member.chaId ? structuredClone(member) : null
        const dialogSnapshot = createChatScreenshotDialogSnapshot({
            characterId: reader.characterId,
            chatId: reader.chatId,
            revision: reader.revision,
            sessionVersion: 1,
            totalTurns: reader.totalTurns,
            renderContext: renderContext(),
        })

        const job = await createChatScreenshotJobFromDialogSnapshot(
            dialogSnapshot,
            reader,
            1,
            1,
        )

        expect(
            job.renderContext.parserContext.database.characters.map(
                (candidate) => candidate.chaId,
            ),
        ).toEqual(['character-1', 'member-1'])
    })

    it('keeps the selected messages and the derived frozen history window needed by CBS', async () => {
        const messages = [
            { role: 'char' as const, data: 'too old' },
            { role: 'user' as const, data: 'previous' },
            { role: 'char' as const, data: 'selected one' },
            { role: 'user' as const, data: 'selected two' },
        ]
        const job = await createProductScreenshotJob(messages, 3, 4)

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

    it('keeps the parser history projection bounded when full history is not requested', async () => {
        const messages = Array.from({ length: 100 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' as const : 'user' as const,
            data: `turn ${index + 1}`,
        }))
        const job = await createProductScreenshotJob(messages, 100, 100)

        expect(job.renderContext.historyStartIndex).toBe(95)
        expect(job.renderContext.parserContext.character.chats[0].message).toHaveLength(5)
        expect(job.messages).toHaveLength(1)
    })
})
