import { describe, expect, it } from 'vitest'
import type { Chat, Message } from './storage/database.svelte'
import { generateResponseCandidate, moveResponseCandidate, recoverInterruptedReroll } from './durableReroll'
import { trackRerollOutput } from './responseVariants'

const message = (data: string, role: Message['role'] = 'char', saying = 'bot'): Message => ({
    data,
    role,
    saying,
    chatId: `message:${data}`,
})
function fixture(messages = [message('question', 'user'), message('original')]) {
    let serial = 0
    const chat: Chat = { id: 'chat', message: messages, name: 'Synthetic', note: '', localLore: [] }
    let persisted = structuredClone(chat)
    const options = {
        chat,
        session: () => null,
        isCurrent: () => true,
        createId: () => `id-${++serial}`,
        flush: async () => {
            persisted = structuredClone(chat)
        },
        aborted: () => false,
        generate: async () => {
            const output = message('new')
            trackRerollOutput(chat, output)
            chat.message.push(output)
            return true
        },
    }
    return { chat, options, persisted: () => persisted }
}

describe('durable response candidates', () => {
    it('continues against the published conversation after a checkpoint replaces its working copy', async () => {
        const f = fixture()
        let current = f.chat
        let saved = structuredClone(current)
        let checkpoints = 0
        await generateResponseCandidate({
            ...f.options,
            currentChat: () => current,
            flush: async () => {
                saved = structuredClone(current)
                if (++checkpoints === 1) current = structuredClone(current)
            },
            generate: async () => {
                const output = message('published result')
                trackRerollOutput(current, output)
                current.message.push(output)
                return true
            },
        })
        expect(saved.message.at(-1)?.data).toBe('published result')
        expect(saved.message.at(-1)?.responseVariants?.candidates).toHaveLength(2)
        expect(saved.rerollRecovery).toBeUndefined()
    })

    it('checkpoints the original before truncation and persists every candidate across reopen', async () => {
        const f = fixture()
        const generate = f.options.generate
        f.options.generate = async () => {
            expect(f.persisted().message.at(-1)?.data).toBe('original')
            expect(f.persisted().rerollRecovery?.original[0].data).toBe('original')
            return generate()
        }
        expect(await generateResponseCandidate(f.options)).toBe(true)
        const reopened = structuredClone(f.persisted())
        expect(reopened.rerollRecovery).toBeUndefined()
        expect(reopened.message.at(-1)?.responseVariants?.candidates).toHaveLength(2)
        expect(moveResponseCandidate(reopened, null, -1, f.options.createId)).toBe(true)
        expect(reopened.message.at(-1)?.data).toBe('original')
        expect(moveResponseCandidate(reopened, null, -1, f.options.createId)).toBe(false)
        expect(moveResponseCandidate(reopened, null, 1, f.options.createId)).toBe(true)
        expect(reopened.message.at(-1)?.data).toBe('new')
    })

    it.each(['fail', 'throw', 'abort', 'navigation', 'empty'] as const)(
        'retains the original selection after %s',
        async (kind) => {
            const f = fixture()
            f.options.generate = async () => {
                const output = message(kind === 'empty' ? '' : 'partial')
                trackRerollOutput(f.chat, output)
                f.chat.message.push(output)
                if (kind === 'throw') throw new Error('synthetic failure')
                if (kind === 'abort') f.options.aborted = () => true
                if (kind === 'navigation') f.options.isCurrent = () => false
                return kind !== 'fail'
            }
            await generateResponseCandidate(f.options).catch(() => false)
            expect(f.chat.message.map((message) => message.data)).toEqual(['question', 'original'])
            expect(f.persisted().message.at(-1)?.data).toBe('original')
            expect(f.chat.message.at(-1)?.responseVariants?.candidates).toHaveLength(1)
        },
    )

    it('recovers a checkpoint saved during a stream without deleting a plugin edit', async () => {
        const f = fixture()
        f.options.generate = async () => {
            const output = message('partial')
            trackRerollOutput(f.chat, output)
            f.chat.message.push(output)
            f.chat.message.at(-1)!.data = 'independent plugin edit'
            await f.options.flush()
            return false
        }
        await generateResponseCandidate(f.options)
        expect(f.chat.message.map((message) => message.data)).toEqual([
            'question',
            'original',
            'independent plugin edit',
        ])
    })

    it('recovers after restart in the middle of streaming', async () => {
        const f = fixture()
        let crashed: Chat
        f.options.generate = async () => {
            const output = message('partial')
            trackRerollOutput(f.chat, output)
            f.chat.message.push(output)
            crashed = structuredClone(f.chat)
            return false
        }
        await generateResponseCandidate(f.options)
        expect(recoverInterruptedReroll(crashed!, null)).toBe(true)
        expect(crashed!.message.map((message) => message.data)).toEqual(['question', 'original'])
        expect(recoverInterruptedReroll(crashed!, null)).toBe(false)
    })

    it('does not generate if the original checkpoint fails to save', async () => {
        const f = fixture()
        f.options.flush = async () => {
            throw new Error('disk full')
        }
        f.options.generate = async () => {
            throw new Error('must not generate')
        }
        await expect(generateResponseCandidate(f.options)).rejects.toThrow('disk full')
        expect(f.chat.message.at(-1)?.data).toBe('original')
    })

    it('appends from an earlier candidate without dropping forward candidates, preserving duplicate text', async () => {
        const f = fixture()
        await generateResponseCandidate(f.options)
        moveResponseCandidate(f.chat, null, -1, f.options.createId)
        await generateResponseCandidate(f.options)
        expect(
            f.chat.message
                .at(-1)
                ?.responseVariants?.candidates.map((candidate) => candidate.messages[0].data),
        ).toEqual(['original', 'new', 'new'])
    })

    it('restores the original projection when the final candidate cannot be saved', async () => {
        const f = fixture()
        const flush = f.options.flush
        let saves = 0
        f.options.flush = async () => {
            if (++saves > 1) throw new Error('disk full')
            await flush()
        }
        await expect(generateResponseCandidate(f.options)).rejects.toThrow('disk full')
        expect(f.chat.message.at(-1)?.data).toBe('original')
        const reopened = structuredClone(f.persisted())
        recoverInterruptedReroll(reopened, null)
        expect(reopened.message.at(-1)?.data).toBe('original')
    })

    it('preserves a variable length group response and trailing comments without a user message', async () => {
        const f = fixture([
            message('a', 'char', 'a'),
            message('b', 'char', 'b'),
            { ...message('note'), isComment: true },
        ])
        await generateResponseCandidate(f.options)
        expect(f.chat.message.map((message) => message.data)).toEqual(['new', 'note'])
        moveResponseCandidate(f.chat, null, -1, f.options.createId)
        expect(f.chat.message.map((message) => message.data)).toEqual(['a', 'b', 'note'])
    })
})
