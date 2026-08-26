import { describe, expect, it, vi } from 'vitest'
import {
    captureStreamingMessageTarget,
    consumeStreamingDisplayStream,
    type StreamingMessageTarget,
    type StreamingDisplayReader,
} from './streamingDisplayStream'

interface TestMessage {
    data: string
}

interface TestChat {
    id: string
    message: TestMessage[]
}

interface TestCharacter {
    chaId: string
    chats: TestChat[]
}

function readerFrom<T>(
    reads: Array<
        ReadableStreamReadResult<T>
        | (() => ReadableStreamReadResult<T> | Promise<ReadableStreamReadResult<T>>)
    >,
) {
    const cancel = vi.fn(async () => undefined)
    const reader: StreamingDisplayReader<T> = {
        cancel,
        async read() {
            const next = reads.shift()
            if (!next) return { done: true, value: undefined }
            return typeof next === 'function' ? next() : next
        },
    }
    return { reader, cancel }
}

function makeCallbacks(target: StreamingMessageTarget<TestCharacter, TestChat, TestMessage>) {
    return {
        processSemantic: async ({ value }: { value: string }, context: { canCommit(): boolean }) => {
            if (context.canCommit()) target.message.data = `semantic:${value}`
        },
        processPreview: async ({ value }: { value: string }, context: { canCommit(): boolean }) => {
            if (context.canCommit()) target.message.data = `preview:${value}`
        },
    }
}

describe('sendChat streaming response boundary', () => {
    it('commits group streaming output to the captured group instead of the speaking member', async () => {
        const group: TestCharacter = {
            chaId: 'group',
            chats: [{ id: 'group-chat', message: [{ data: '' }] }],
        }
        const speakingMember: TestCharacter = {
            chaId: 'member',
            chats: [{ id: 'member-chat', message: [] }],
        }
        const characters = [group, speakingMember]
        const target = captureStreamingMessageTarget<TestCharacter, TestChat, TestMessage>(
            () => characters,
            0,
            0,
            0,
        )
        const { reader } = readerFrom([
            { done: false, value: { text: 'hello' } },
            { done: true, value: { text: 'hello group' } },
        ])

        const result = await consumeStreamingDisplayStream({
            mode: 'balanced',
            reader,
            abortSignal: new AbortController().signal,
            getSnapshot: (value) => value.text,
            isOwned: target.isOwned,
            ...makeCallbacks(target),
        })

        expect(speakingMember.chaId).not.toBe(target.character.chaId)
        expect(target.character).toBe(group)
        expect(group.chats[0].message[0].data).toBe('semantic:hello group')
        expect(result.completed).toBe(true)
    })

    it('processes a final provider value that arrives together with done', async () => {
        const character: TestCharacter = {
            chaId: 'character',
            chats: [{ id: 'chat', message: [{ data: '' }] }],
        }
        const target = captureStreamingMessageTarget<TestCharacter, TestChat, TestMessage>(
            () => [character],
            0,
            0,
            0,
        )
        const semanticValues: string[] = []
        const previewValues: string[] = []
        const { reader } = readerFrom([
            { done: true, value: { text: 'final' } },
        ])

        const result = await consumeStreamingDisplayStream({
            mode: 'strong',
            reader,
            abortSignal: new AbortController().signal,
            getSnapshot: (value) => value.text,
            isOwned: target.isOwned,
            processPreview: async ({ value }, context) => {
                if (context.canCommit()) previewValues.push(value)
            },
            processSemantic: async ({ value }, context) => {
                if (context.canCommit()) semanticValues.push(value)
            },
        })

        expect(previewValues).toEqual(['final'])
        expect(semanticValues).toEqual(['final'])
        expect(result).toMatchObject({ completed: true, latestSnapshot: 'final' })
    })

    it('propagates reader failure without converting it into normal EOF', async () => {
        const character: TestCharacter = {
            chaId: 'character',
            chats: [{ id: 'chat', message: [{ data: 'previous' }] }],
        }
        const target = captureStreamingMessageTarget<TestCharacter, TestChat, TestMessage>(
            () => [character],
            0,
            0,
            0,
        )
        const failure = new Error('reader failed')
        const cancel = vi.fn(async () => undefined)
        const reader: StreamingDisplayReader<{ text: string }> = {
            cancel,
            async read() {
                throw failure
            },
        }
        const callbacks = makeCallbacks(target)

        await expect(consumeStreamingDisplayStream({
            mode: 'balanced',
            reader,
            abortSignal: new AbortController().signal,
            getSnapshot: (value) => value.text,
            isOwned: target.isOwned,
            ...callbacks,
        })).rejects.toBe(failure)

        expect(character.chats[0].message[0].data).toBe('previous')
        expect(cancel).toHaveBeenCalledTimes(1)
    })

    it('drops a late commit when the captured message owner is replaced', async () => {
        let characters: TestCharacter[] = [{
            chaId: 'character',
            chats: [{ id: 'chat', message: [{ data: 'previous' }] }],
        }]
        const original = characters[0]
        const target = captureStreamingMessageTarget<TestCharacter, TestChat, TestMessage>(
            () => characters,
            0,
            0,
            0,
        )
        let release!: () => void
        const active = new Promise<void>((resolve) => { release = resolve })
        const { reader } = readerFrom([
            { done: false, value: { text: 'late' } },
            () => {
                characters = [{
                    chaId: 'character',
                    chats: [{ id: 'chat', message: [{ data: 'replacement' }] }],
                }]
                release()
                return { done: true, value: undefined }
            },
        ])

        const result = await consumeStreamingDisplayStream({
            mode: 'balanced',
            reader,
            abortSignal: new AbortController().signal,
            getSnapshot: (value) => value.text,
            isOwned: target.isOwned,
            processSemantic: async ({ value }, context) => {
                await active
                if (context.canCommit()) target.message.data = value
            },
            processPreview: async () => {},
        })

        expect(original.chats[0].message[0].data).toBe('previous')
        expect(characters[0].chats[0].message[0].data).toBe('replacement')
        expect(result.completed).toBe(false)
    })
})
