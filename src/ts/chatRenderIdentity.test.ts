import { describe, expect, it } from 'vitest'
import type { Message } from './storage/database.svelte'
import {
    areChatRenderSignaturesEqual,
    ChatRenderIdentityRegistry,
    createChatParserDependencyStamp,
    createChatRenderSignature,
} from './chatRenderIdentity'

function message(chatId: string | undefined, data = 'hello'): Message {
    return { role: 'char', data, chatId }
}

const defaultParserCharacter = {
    chaId: 'character-a',
    virtualscript: 'virtual',
    customscript: [],
    additionalAssets: [],
    emotionImages: [],
    triggerscript: [],
}

function signatureFor(value: Message, overrides: Partial<Parameters<typeof createChatRenderSignature>[0]> = {}) {
    const input = {
        message: value,
        index: 1,
        totalLength: 3,
        largePortrait: false,
        reloadPointer: 0,
        activeStreamingMessage: false,
        resolvedImage: 'background:character.png',
        displayName: 'Character',
        globalReloadPointer: 0,
        parserCharacter: defaultParserCharacter,
        ...overrides,
    }
    return createChatRenderSignature({
        ...input,
        parserCharacterStamp: createChatParserDependencyStamp(input.parserCharacter),
    })
}

const sameSignature = (
    left: ReturnType<typeof createChatRenderSignature>,
    right: ReturnType<typeof createChatRenderSignature>,
) => areChatRenderSignaturesEqual(left, right)

describe('ChatRenderIdentityRegistry', () => {
    it('uses a unique chatId as stable identity across edits, rerolls, and index moves', () => {
        const registry = new ChatRenderIdentityRegistry()
        const original = message('message-1')
        const edited = message('message-1', 'edited')
        const rerolled = { ...edited, generationInfo: { generationId: 'reroll-2' } }

        const originalKey = registry.resolve('conversation-a', [original])[0]
        expect(registry.resolve('conversation-a', [edited])[0]).toBe(originalKey)
        expect(registry.resolve('conversation-a', [message('before'), rerolled])[1]).toBe(originalKey)
        expect(registry.resolve('conversation-b', [rerolled])[0]).not.toBe(originalKey)
    })

    it('keeps duplicate and missing legacy IDs distinct without reusing state after reordering', () => {
        const registry = new ChatRenderIdentityRegistry()
        const duplicateA = message('duplicate', 'a')
        const duplicateB = message('duplicate', 'b')
        const missingA = message(undefined, 'c')
        const missingB = message(undefined, 'd')
        const first = registry.resolve('conversation-a', [duplicateA, duplicateB, missingA, missingB])
        const reordered = registry.resolve('conversation-a', [missingB, duplicateB, duplicateA, missingA])

        expect(new Set(first).size).toBe(4)
        expect(reordered).toEqual([first[3], first[1], first[0], first[2]])

        const replacements = registry.resolve('conversation-a', [
            message('duplicate', 'a'),
            message('duplicate', 'b'),
            message(undefined, 'c'),
        ])
        expect(replacements.every((key) => !first.includes(key))).toBe(true)
    })

    it('assigns unique occurrence keys when the identical legacy object appears twice', () => {
        const registry = new ChatRenderIdentityRegistry()
        const repeated = message(undefined, 'same object')
        const keys = registry.resolve('conversation-a', [repeated, repeated])

        expect(new Set(keys).size).toBe(2)
        expect(registry.resolve('conversation-a', [repeated, repeated])).toEqual(keys)
    })
})

describe('createChatRenderSignature', () => {
    it('separates stable identity from content and explicit render dependencies', () => {
        const original = message('message-1')
        const base = signatureFor(original)

        expect(sameSignature(signatureFor({ ...original, data: 'edited' }), base)).toBe(false)
        expect(sameSignature(signatureFor({ ...original, generationInfo: { generationId: 'reroll-2' } }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, { index: 2 }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, { reloadPointer: 1 }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, { resolvedImage: 'changed.png' }), base)).toBe(false)
    })

    it('reuses the render signature during optimized streaming and remounts when streaming settles', () => {
        const firstChunk = message('stream', 'first')
        const secondChunk = message('stream', 'first second')

        const first = signatureFor(firstChunk, { activeStreamingMessage: true })
        const second = signatureFor(secondChunk, { activeStreamingMessage: true })
        const settled = signatureFor(secondChunk, { activeStreamingMessage: false })

        expect(sameSignature(second, first)).toBe(true)
        expect(sameSignature(settled, first)).toBe(false)
    })

    it('does not invalidate settled history when messages are appended to the live tail', () => {
        const settled = message('settled')
        const beforeAppend = signatureFor(settled, { index: 10, totalLength: 100 })
        const afterAppend = signatureFor(settled, { index: 10, totalLength: 101 })

        expect(sameSignature(afterAppend, beforeAppend)).toBe(true)
    })

    it('retains content by reference and tracks parser dependency identities and reload revisions', () => {
        const original = message('message-1', 'long message content')
        const parserCharacter = signatureFor(original).parserCharacter
        const base = signatureFor(original, { parserCharacter })

        expect(base.content).toBe(original.data)
        expect(sameSignature(signatureFor(original, {
            parserCharacter: { ...parserCharacter, customscript: [] },
        }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, {
            parserCharacter: { ...parserCharacter, virtualscript: 'changed' },
        }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, { globalReloadPointer: 1, parserCharacter }), base)).toBe(false)
    })

    it('tracks in-place parser asset and script mutations without replacing their arrays', () => {
        const original = message('message-1')
        const parserCharacter = {
            ...defaultParserCharacter,
            additionalAssets: [['Portrait', 'portrait.png', 'png']],
            customscript: [{ type: 'editdisplay', in: 'before', out: 'after' }],
        }
        const beforeAssetEdit = signatureFor(original, { parserCharacter })

        parserCharacter.additionalAssets[0][1] = 'changed.png'
        const afterAssetEdit = signatureFor(original, { parserCharacter })
        expect(sameSignature(afterAssetEdit, beforeAssetEdit)).toBe(false)

        parserCharacter.customscript[0].out = 'changed'
        expect(sameSignature(signatureFor(original, { parserCharacter }), afterAssetEdit)).toBe(false)
    })
})
