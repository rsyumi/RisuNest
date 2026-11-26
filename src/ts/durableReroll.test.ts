import { describe, expect, it } from 'vitest'
import type { Chat, Message } from './storage/database.svelte'
import {
    generateResponseCandidate,
    generateWindowedResponseCandidate,
    moveResponseCandidate,
    moveWindowedResponseCandidate,
    recoverInterruptedReroll,
    type OpenResponseTail,
} from './durableReroll'
import { attachHistoryWindow } from './process/historyWindowIndex'
import { captureResponseVariants, projectResponseVariant, recoverRerollMessages, rerollAfterPluginRanges, trackRerollOutput } from './responseVariants'

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
    it.each([true, false])('keeps the surviving prefix after the reroll anchor is deleted, generation succeeds=%s', async success => {
        const f = fixture([message('old', 'user'), message('keep', 'user'), message('question', 'user'), message('original')])
        f.options.generate = async () => {
            const output = message('next')
            trackRerollOutput(f.chat, output)
            f.chat.message.push(output)
            f.chat.message.splice(2, 1)
            f.chat.message[0] = message('earlier', 'user')
            return success
        }
        expect(await generateResponseCandidate(f.options)).toBe(success)
        expect(f.persisted().message.map(row => row.data)).toEqual(['earlier', 'keep', success ? 'next' : 'original'])
        if (success) {
            expect(f.persisted().message.at(-1)?.responseVariants?.candidates.at(-1)?.messages.map(row => row.data)).toEqual(['next'])
            expect(moveResponseCandidate(f.chat, null, -1, f.options.createId)).toBe(true)
            expect(f.chat.message.map(row => row.data)).toEqual(['earlier', 'keep', 'original'])
        }
    })

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
        expect(Object.hasOwn(f.chat, 'rerollRecovery')).toBe(false)
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

    it('preserves a variable length response and trailing comments without a user message', async () => {
        const original = [message('a'), message('b')]
        const variants = captureResponseVariants(original, () => 'original')
        const f = fixture([
            ...projectResponseVariant(variants, variants.selectedId),
            { ...message('note'), isComment: true },
        ])
        await generateResponseCandidate(f.options)
        expect(f.chat.message.map((message) => message.data)).toEqual(['new', 'note'])
        moveResponseCandidate(f.chat, null, -1, f.options.createId)
        expect(f.chat.message.map((message) => message.data)).toEqual(['a', 'b', 'note'])
    })
})

// A stored conversation that tail windows read from and write back to by absolute index.
function storedConversation(messages: Message[]) {
    const stored = { messages, metadata: { id: 'chat', name: 'Synthetic', note: '', localLore: [] } as Omit<Chat, 'message'> }
    const starts: number[] = []
    let flushed = structuredClone(stored)
    const open: OpenResponseTail = async (tailStart) => {
        const start = Math.min(Math.max(tailStart(stored.messages.length), 0), stored.messages.length)
        starts.push(start)
        const chat = { ...structuredClone(stored.metadata), message: structuredClone(stored.messages.slice(start)) } as Chat
        attachHistoryWindow(chat, start)
        let released = false
        return {
            chat,
            controller: {
                chat,
                absoluteStartIndex: start,
                isCurrent: () => !released,
                applyRange(localStart, deleteCount, replacement) {
                    if (released) return false
                    stored.messages.splice(start + localStart, deleteCount, ...structuredClone([...replacement]))
                    chat.message.splice(localStart, deleteCount, ...structuredClone([...replacement]))
                    const { message: _message, ...metadata } = chat
                    stored.metadata = structuredClone(metadata)
                    return true
                },
                release: () => {
                    released = true
                },
            },
            release: () => {
                released = true
            },
        }
    }
    let serial = 0
    const options = {
        open,
        isCurrent: () => true,
        createId: () => `id-${++serial}`,
        flush: async () => {
            flushed = structuredClone(stored)
        },
        aborted: () => false,
        generate: async () => {
            stored.messages.push(message('new'))
            return true
        },
    }
    return { stored, starts, options, flushed: () => flushed }
}

const longConversation = (count: number) =>
    Array.from({ length: count }, (_, index) => message(`m${index}`, index % 2 ? 'char' : 'user'))

describe('response candidates over a history window', () => {
    it.each([true, false])('preserves both sides of a relocated response after generation succeeds=%s', async success => {
        const conversation = storedConversation(longConversation(100))
        const stored = conversation.stored
        const earlier = stored.messages.slice(20, 98)
        const independent = message('independent', 'user')
        conversation.options.generate = async () => {
            const output = message('new')
            const chat = { ...stored.metadata, message: stored.messages } as Chat
            trackRerollOutput(chat, output, stored.messages.length)
            stored.messages.push(output)
            const ranges = [
                { start: 0, before: stored.messages.slice(0, 20), messages: [] },
                { start: 78, before: [stored.messages[98]], messages: [] },
                { start: 79, before: [], messages: [independent] },
            ]
            stored.metadata.rerollRecovery = rerollAfterPluginRanges(chat.rerollRecovery, ranges)
            for (const range of ranges) stored.messages.splice(range.start, range.before.length, ...range.messages)
            return success
        }
        expect((await generateWindowedResponseCandidate(conversation.options))?.completed).toBe(success)
        const persisted = conversation.flushed().messages
        expect(persisted.slice(0, -2)).toEqual(earlier)
        expect(persisted.at(-1)).toEqual(independent)
        const carrier = persisted.at(-2)
        expect(carrier.data).toBe(success ? 'new' : 'm99')
        expect(carrier.responseVariants.candidates.map(candidate => candidate.messages.map(row => row.data)))
            .toEqual(success ? [['m99'], ['new']] : [['m99']])
    })

    it('generates a candidate from tail windows and leaves earlier messages untouched', async () => {
        const conversation = storedConversation(longConversation(100))
        const before = structuredClone(conversation.stored.messages.slice(0, 99))
        conversation.options.generate = async () => {
            expect(conversation.flushed().messages.at(-1)?.data).toBe('m99')
            expect(conversation.stored.messages.at(-1)?.data).toBe('m98')
            expect(conversation.stored.metadata.rerollRecovery).toMatchObject({
                phase: 'generating',
                startIndex: 99,
                anchorId: 'message:m98',
            })
            conversation.stored.messages.push(message('new'))
            return true
        }

        const result = await generateWindowedResponseCandidate(conversation.options)

        expect(result).toEqual({ completed: true, lastMessage: 'new' })
        expect(conversation.stored.messages.slice(0, 99)).toEqual(before)
        expect(conversation.stored.messages.at(-1)?.responseVariants?.candidates.map((candidate) => candidate.messages[0].data))
            .toEqual(['m99', 'new'])
        expect(conversation.stored.metadata.rerollRecovery).toBeUndefined()
        expect(conversation.flushed().messages.at(-1)?.data).toBe('new')
        expect(Math.min(...conversation.starts)).toBe(92)
    })

    it('restores the original response when the generation fails', async () => {
        const conversation = storedConversation(longConversation(100))
        const before = structuredClone(conversation.stored.messages.slice(0, 99))
        conversation.options.generate = async () => {
            // The generation records its output in the stored recovery as it writes.
            const output = message('partial')
            conversation.stored.metadata.rerollRecovery!.outputs[output.chatId!] = structuredClone(output)
            conversation.stored.messages.push(output)
            return false
        }
        expect(await generateWindowedResponseCandidate(conversation.options)).toEqual({ completed: false, lastMessage: 'm99' })
        expect(conversation.stored.messages.slice(0, 99)).toEqual(before)
        expect(conversation.stored.messages.map((entry) => entry.data).slice(98)).toEqual(['m98', 'm99'])
        expect(conversation.stored.metadata.rerollRecovery).toBeUndefined()
    })

    it('returns null when no tail window opens', async () => {
        const conversation = storedConversation(longConversation(10))
        expect(await generateWindowedResponseCandidate({
            ...conversation.options,
            open: async () => null,
            generate: async () => {
                throw new Error('must not generate')
            },
        })).toBeNull()
    })

    it('keeps the stored recovery when the tail cannot be reopened after the generation', async () => {
        const conversation = storedConversation(longConversation(100))
        const open = conversation.options.open
        let generated = false
        const result = await generateWindowedResponseCandidate({
            ...conversation.options,
            open: async (tailStart) => generated ? null : open(tailStart),
            generate: async () => {
                const output = message('new')
                conversation.stored.metadata.rerollRecovery!.outputs[output.chatId!] = structuredClone(output)
                conversation.stored.messages.push(output)
                generated = true
                return true
            },
        })

        expect(result).toEqual({ completed: false, reopenFailed: true })
        expect(conversation.stored.metadata.rerollRecovery).toMatchObject({ phase: 'generating', startIndex: 99 })
        const reopened = { ...structuredClone(conversation.stored.metadata), message: structuredClone(conversation.stored.messages) } as Chat
        expect(recoverRerollMessages(reopened).slice(98).map((entry) => entry.data)).toEqual(['m98', 'm99'])
    })

    it('does nothing when the last message changed after confirmation', async () => {
        const conversation = storedConversation(longConversation(10))
        const before = structuredClone(conversation.stored)
        const result = await generateWindowedResponseCandidate({
            ...conversation.options,
            expectedLastMessage: JSON.stringify(message('other')),
            generate: async () => {
                throw new Error('must not generate')
            },
        })
        expect(result).toEqual({ completed: false })
        expect(conversation.stored).toEqual(before)
    })

    it('widens the tail until it holds the message before a long response', async () => {
        const response = Array.from({ length: 10 }, (_, index) => message(`r${index}`))
        const variants = captureResponseVariants(response, () => 'original')
        const conversation = storedConversation([
            ...longConversation(20),
            ...projectResponseVariant(variants, variants.selectedId),
        ])
        const { open, createId, flush } = conversation.options
        const move = await moveWindowedResponseCandidate(open, 1, createId, flush)
        expect(move).toEqual({ moved: false, lastMessage: JSON.stringify(conversation.stored.messages.at(-1)) })
        expect(conversation.starts).toEqual([22, 14])
    })

    it('reads one tail window when the newest message is not a response', async () => {
        const conversation = storedConversation([...longConversation(1000), message('question', 'user')])
        const before = structuredClone(conversation.stored)
        const { open, createId, flush } = conversation.options

        expect(await moveWindowedResponseCandidate(open, 1, createId, flush)).toEqual({ moved: false, lastMessage: null })
        expect(await moveWindowedResponseCandidate(open, -1, createId, flush)).toEqual({ moved: false, lastMessage: null })
        expect(await generateWindowedResponseCandidate({
            ...conversation.options,
            generate: async () => {
                throw new Error('must not generate')
            },
        })).toMatchObject({ completed: false })

        expect(conversation.starts).toEqual([993, 993, 993])
        expect(conversation.stored).toEqual(before)
    })

    it('widens past a tail of comments and disabled messages to the response', async () => {
        const conversation = storedConversation([
            ...longConversation(100),
            ...Array.from({ length: 6 }, (_, index) => ({ ...message(`note${index}`), isComment: true })),
            ...Array.from({ length: 4 }, (_, index) => ({ ...message(`off${index}`, 'user'), disabled: true })),
        ])
        const { open, createId, flush } = conversation.options

        const move = await moveWindowedResponseCandidate(open, 1, createId, flush)

        expect(move).toEqual({ moved: false, lastMessage: JSON.stringify(conversation.stored.messages.at(-1)) })
        expect(conversation.starts).toEqual([102, 94])
    })

    it('opens a conversation shorter than the tail once', async () => {
        const conversation = storedConversation(longConversation(3))
        const { open, createId, flush } = conversation.options

        expect(await moveWindowedResponseCandidate(open, 1, createId, flush)).toEqual({ moved: false, lastMessage: null })
        expect(conversation.starts).toEqual([0])
    })

    it('moves between candidates over a tail window', async () => {
        const conversation = storedConversation(longConversation(100))
        await generateWindowedResponseCandidate(conversation.options)
        const { open, createId, flush } = conversation.options
        expect(await moveWindowedResponseCandidate(open, -1, createId, flush)).toEqual({ moved: true, lastMessage: null })
        expect(conversation.flushed().messages.at(-1)?.data).toBe('m99')
        expect(await moveWindowedResponseCandidate(open, -1, createId, flush)).toEqual({
            moved: false,
            lastMessage: JSON.stringify(conversation.stored.messages.at(-1)),
        })
        expect(await moveWindowedResponseCandidate(open, 1, createId, flush)).toEqual({ moved: true, lastMessage: null })
        expect(conversation.stored.messages.at(-1)?.data).toBe('new')
        expect(conversation.stored.messages).toHaveLength(100)
    })

    it('recovers a window by its absolute start when the anchor is missing', () => {
        const chat = {
            id: 'chat',
            message: [message('a', 'user'), message('partial')],
            rerollRecovery: {
                attemptId: 'attempt',
                phase: 'generating',
                startIndex: 51,
                original: [message('original')],
                responseCount: 1,
                outputs: { 'message:partial': message('partial') },
            },
        } as unknown as Chat
        attachHistoryWindow(chat, 50)
        expect(recoverRerollMessages(chat).map((entry) => entry.data)).toEqual(['a', 'original'])
    })
})
