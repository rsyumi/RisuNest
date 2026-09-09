import { describe, expect, it, vi } from 'vitest'
import type { Message } from './storage/database.svelte'
import { buildChatViewport } from './chatViewport'
import {
    areChatRenderSignaturesEqual,
    canRefreshChatRenderInPlace,
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
    it('allows content and history refreshes while retaining presentation and identity boundaries', () => {
        const before = signatureFor(message('id', 'before'))
        expect(canRefreshChatRenderInPlace(before, before)).toBe(true)
        expect(
            canRefreshChatRenderInPlace(
                before,
                signatureFor(message('id', 'after')),
            ),
        ).toBe(true)
        expect(
            canRefreshChatRenderInPlace(
                before,
                signatureFor(message('id', 'after'), { totalLength: 4 }),
            ),
        ).toBe(true)
        expect(
            canRefreshChatRenderInPlace(
                before,
                signatureFor({ ...message('id'), role: 'user' }),
            ),
        ).toBe(false)
        expect(
            canRefreshChatRenderInPlace(
                before,
                signatureFor(message('id'), { index: 2 }),
            ),
        ).toBe(false)
        expect(
            canRefreshChatRenderInPlace(
                before,
                signatureFor(message('id'), { resolvedImage: 'changed' }),
            ),
        ).toBe(false)
        expect(
            canRefreshChatRenderInPlace(
                before,
                signatureFor(message('id'), {
                    parserCharacter: {
                        ...defaultParserCharacter,
                        virtualscript: 'changed',
                    },
                }),
            ),
        ).toBe(false)
    })

    it('keeps an id-less identity when a chat ID is assigned later', () => {
        const registry = new ChatRenderIdentityRegistry()
        const original = message(undefined, 'first')
        const messages = [original]
        const missingKey = registry.register('conversation-a', messages).keyAt(0)

        original.chatId = 'assigned-later'
        const assignedKey = registry.register('conversation-a', messages).keyAt(0)

        expect(assignedKey).toBe(missingKey)

        messages.push(message('assigned-later', 'second'))
        const duplicateKeys = registry.registerAppend('conversation-a', messages, 1).toArray()

        expect(new Set(duplicateKeys).size).toBe(2)
        expect(duplicateKeys[0]).toBe(missingKey)
    })

    it('registers a 10,000-message identity sequence once and resolves viewport rows without rescanning messages', () => {
        const registry = new ChatRenderIdentityRegistry()
        const source = Array.from({ length: 10_000 }, (_, index) => message(`message-${index}`))
        let indexedReads = 0
        const messages = new Proxy(source, {
            get(target, property, receiver) {
                if (typeof property === 'string' && /^\d+$/.test(property)) indexedReads++
                return Reflect.get(target, property, receiver)
            },
        })

        const sequence = registry.register('conversation-a', messages)
        expect(indexedReads).toBeGreaterThanOrEqual(10_000)

        indexedReads = 0
        expect(sequence.keysAt([100, 5000, 9999])).toEqual([
            registry.resolve('conversation-a', [source[100]])[0],
            registry.resolve('conversation-a', [source[5000]])[0],
            registry.resolve('conversation-a', [source[9999]])[0],
        ])
        expect(indexedReads).toBe(0)
    })

    it('registers only an appended suffix when the earlier message objects are unchanged', () => {
        const registry = new ChatRenderIdentityRegistry()
        const reads = [0, 0, 0, 0]
        const countedMessage = (index: number): Message => {
            const value = { role: 'char' as const, data: `message ${index}` } as Message
            Object.defineProperty(value, 'chatId', {
                get() {
                    reads[index]++
                    return `message-${index}`
                },
            })
            return value
        }
        const messages = [countedMessage(0), countedMessage(1), countedMessage(2)]
        const before = registry.register('conversation-a', messages).toArray()
        reads.fill(0)

        messages.push(countedMessage(3))
        const after = registry.registerAppend('conversation-a', messages, 3).toArray()

        expect(reads.slice(0, 3)).toEqual([0, 0, 0])
        expect(reads[3]).toBeGreaterThan(0)
        expect(after.slice(0, 3)).toEqual(before)
        expect(new Set(after).size).toBe(4)
    })

    it('does not read prior array indices when the caller declares an append', () => {
        const registry = new ChatRenderIdentityRegistry()
        const source = Array.from({ length: 10_000 }, (_, index) => message(`message-${index}`))
        let priorIndexedReads = 0
        let suffixIndexedReads = 0
        const messages = new Proxy(source, {
            get(target, property, receiver) {
                if (typeof property === 'string' && /^\d+$/.test(property)) {
                    if (Number(property) < 10_000) priorIndexedReads++
                    else suffixIndexedReads++
                }
                return Reflect.get(target, property, receiver)
            },
        })
        registry.register('conversation-a', messages)
        priorIndexedReads = 0
        suffixIndexedReads = 0

        source.push(message('message-10000'))
        const sequence = registry.registerAppend('conversation-a', messages, 10_000)

        expect(sequence.keyAt(10_000)).toBeDefined()
        expect(priorIndexedReads).toBe(0)
        expect(suffixIndexedReads).toBeGreaterThan(0)
    })

    it('keeps the issued identity when an append turns a unique chat ID into a duplicate', () => {
        const registry = new ChatRenderIdentityRegistry()
        const original = message('duplicate', 'first')
        const messages = [original]
        const uniqueKey = registry.register('conversation-a', messages).keyAt(0)

        messages.push(message('duplicate', 'second'))
        const duplicateKeys = registry.register('conversation-a', messages).toArray()

        expect(new Set(duplicateKeys).size).toBe(2)
        expect(duplicateKeys[0]).toBe(uniqueKey)

        messages.pop()
        expect(registry.register('conversation-a', messages).keyAt(0)).toBe(uniqueKey)
    })

    it('keeps a viewport anchor on the original message when a duplicate is inserted before it', () => {
        const registry = new ChatRenderIdentityRegistry()
        const originalDuplicate = message('duplicate', 'original')
        const messages = [message('x'), originalDuplicate, message('y')]
        const beforeKeys = registry.register('conversation-a', messages).toArray()
        const anchor = {
            key: beforeKeys[1],
            indexHint: 1,
            relativeOffset: 17,
        }

        messages.splice(1, 0, message('duplicate', 'inserted'))
        const afterKeys = registry.register('conversation-a', messages).toArray()
        const insertedFallback = afterKeys[1]
        const viewport = buildChatViewport({
            keys: afterKeys,
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 100,
            anchor,
        })

        expect(afterKeys[2]).toBe(anchor.key)
        expect(afterKeys[1]).not.toBe(anchor.key)
        expect(viewport.anchor).toEqual({ ...anchor, indexHint: 2 })

        messages.splice(2, 1)
        registry.register('conversation-a', messages)
        messages.splice(2, 0, originalDuplicate)
        const reinsertedKeys = registry.register('conversation-a', messages).toArray()
        expect(reinsertedKeys[1]).toBe(insertedFallback)
        expect(reinsertedKeys[2]).toBe(anchor.key)
    })

    it('keeps a duplicate fallback identity after the original chat-key owner is deleted', () => {
        const registry = new ChatRenderIdentityRegistry()
        const original = message('duplicate', 'original')
        const inserted = message('duplicate', 'inserted')
        const messages = [message('x'), original, message('y')]
        registry.register('conversation-a', messages)

        messages.splice(1, 0, inserted)
        const duplicateKeys = registry.register('conversation-a', messages).toArray()
        const anchor = {
            key: duplicateKeys[1],
            indexHint: 1,
            relativeOffset: 13,
        }

        messages.splice(2, 1)
        const afterDeletion = registry.register('conversation-a', messages).toArray()
        const viewport = buildChatViewport({
            keys: afterDeletion,
            budget: 3,
            overscan: 1,
            estimatedMessageHeight: 100,
            anchor,
        })

        expect(afterDeletion[1]).toBe(anchor.key)
        expect(viewport.anchor).toEqual(anchor)
    })

    it('reuses an id-less message identity after deletion and reinsertion', () => {
        const registry = new ChatRenderIdentityRegistry()
        const idless = message(undefined, 'id-less')
        const messages = [message('x'), idless, message('y')]
        const originalKey = registry.register('conversation-a', messages).keyAt(1)

        messages.splice(1, 1)
        registry.register('conversation-a', messages)
        messages.splice(1, 0, idless)

        expect(registry.register('conversation-a', messages).keyAt(1)).toBe(originalKey)
    })

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
    it('reuses immutable large script fingerprints while detecting same-length in-place edits', () => {
        const source = 'synthetic-lua-dependency:'.repeat(1024)
        const character = {
            ...defaultParserCharacter,
            triggerscript: [{ effect: [{ type: 'triggerlua', code: source }] }],
        }
        const initial = createChatParserDependencyStamp(character)
        const multiply = vi.spyOn(Math, 'imul')
        try {
            expect(createChatParserDependencyStamp(character)).toBe(initial)
            expect(multiply.mock.calls.length).toBeLessThan(2_000)

            multiply.mockClear()
            character.triggerscript[0].effect[0].code = `${source.slice(0, -1)}!`
            const updated = createChatParserDependencyStamp(character)
            expect(updated).not.toBe(initial)
            expect(multiply.mock.calls.length).toBeGreaterThan(source.length)

            multiply.mockClear()
            expect(createChatParserDependencyStamp(character)).toBe(updated)
            expect(multiply.mock.calls.length).toBeLessThan(2_000)
        } finally {
            multiply.mockRestore()
        }
    })

    it('evicts retained large script strings by total size without changing their stamps', () => {
        const character = {
            ...defaultParserCharacter,
            customscript: [{ out: 'synthetic-cache-size-probe:'.repeat(256) }],
        }
        const initial = createChatParserDependencyStamp(character)
        for (let index = 0; index < 3; index++) {
            createChatParserDependencyStamp({
                ...defaultParserCharacter,
                customscript: [
                    {
                        out: `synthetic-large-cache-entry-${index}:`.padEnd(
                            800_000,
                            'x',
                        ),
                    },
                ],
            })
        }
        const multiply = vi.spyOn(Math, 'imul')
        try {
            expect(createChatParserDependencyStamp(character)).toBe(initial)
            expect(multiply.mock.calls.length).toBeGreaterThan(
                character.customscript[0].out.length,
            )
        } finally {
            multiply.mockRestore()
        }
    })

    it('separates stable identity from content and explicit render dependencies', () => {
        const original = message('message-1')
        const base = signatureFor(original)

        expect(sameSignature(signatureFor({ ...original, data: 'edited' }), base)).toBe(false)
        expect(sameSignature(signatureFor({ ...original, generationInfo: { generationId: 'reroll-2' } }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, { index: 2 }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, { reloadPointer: 1 }), base)).toBe(false)
        expect(sameSignature(signatureFor(original, { bookmarked: true }), base)).toBe(false)
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
