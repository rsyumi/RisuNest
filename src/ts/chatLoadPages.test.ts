import { describe, expect, it } from 'vitest'
import {
    DEFAULT_CHAT_LOAD_ADDITIONAL_PAGES,
    DEFAULT_CHAT_LOAD_INITIAL_PAGES,
    normalizeChatLoadPages,
} from './chatLoadPages'

describe('normalizeChatLoadPages', () => {
    it('keeps positive finite counts as integers', () => {
        expect(normalizeChatLoadPages(42, DEFAULT_CHAT_LOAD_INITIAL_PAGES)).toBe(42)
        expect(normalizeChatLoadPages(7.9, DEFAULT_CHAT_LOAD_INITIAL_PAGES)).toBe(7)
    })

    it('falls back for invalid counts', () => {
        expect(normalizeChatLoadPages(0, DEFAULT_CHAT_LOAD_INITIAL_PAGES)).toBe(DEFAULT_CHAT_LOAD_INITIAL_PAGES)
        expect(normalizeChatLoadPages(-1, DEFAULT_CHAT_LOAD_INITIAL_PAGES)).toBe(DEFAULT_CHAT_LOAD_INITIAL_PAGES)
        expect(normalizeChatLoadPages(Infinity, DEFAULT_CHAT_LOAD_INITIAL_PAGES)).toBe(DEFAULT_CHAT_LOAD_INITIAL_PAGES)
        expect(normalizeChatLoadPages(Number.NaN, DEFAULT_CHAT_LOAD_INITIAL_PAGES)).toBe(DEFAULT_CHAT_LOAD_INITIAL_PAGES)
        expect(normalizeChatLoadPages('', DEFAULT_CHAT_LOAD_INITIAL_PAGES)).toBe(DEFAULT_CHAT_LOAD_INITIAL_PAGES)
    })
})
