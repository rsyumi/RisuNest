import { describe, expect, it } from 'vitest'
import {
    DEFAULT_CHAT_LOAD_ADDITIONAL_PAGES,
    DEFAULT_CHAT_LOAD_INITIAL_PAGES,
    getAdditionalChatLoadPages,
    getInitialChatLoadPages,
    normalizeChatLoadPages,
    shouldContainChatMessage,
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

    it('uses built-in defaults for chat load settings', () => {
        expect(getInitialChatLoadPages({})).toBe(DEFAULT_CHAT_LOAD_INITIAL_PAGES)
        expect(getInitialChatLoadPages({ chatLoadInitialPages: 12 })).toBe(12)
        expect(getAdditionalChatLoadPages({})).toBe(DEFAULT_CHAT_LOAD_ADDITIONAL_PAGES)
        expect(getAdditionalChatLoadPages({ chatLoadAdditionalPages: 8 })).toBe(8)
    })
})

describe('shouldContainChatMessage', () => {
    const settledHistory = {
        index: 1,
        totalLength: 3,
        isStreaming: false,
        isComment: false,
        data: 'Ordinary settled text',
        captureAll: false,
    }

    it.each([
        ['ordinary old text', settledHistory, true],
        ['newest message', { ...settledHistory, index: 2 }, false],
        ['streaming newest message', { ...settledHistory, index: 2, isStreaming: true }, false],
        ['comment', { ...settledHistory, isComment: true }, false],
        ['button markup', { ...settledHistory, data: '<button>Run</button>' }, false],
        ['audio markup', { ...settledHistory, data: '<audio controls></audio>' }, false],
        ['video markup', { ...settledHistory, data: '<video></video>' }, false],
        ['inlay markup', { ...settledHistory, data: '{{inlayed::asset-id}}' }, false],
        ['audio marker', { ...settledHistory, data: '{{audio::asset-id}}' }, false],
        ['video marker', { ...settledHistory, data: '{{video::asset-id}}' }, false],
        ['video image marker', { ...settledHistory, data: '{{video-img::asset-id}}' }, false],
        ['background music marker', { ...settledHistory, data: '{{bgm::asset-id}}' }, false],
        ['asset marker', { ...settledHistory, data: '{{asset::asset-id}}' }, false],
        ['button marker', { ...settledHistory, data: '{{button::Run}}' }, false],
        ['iframe markup', { ...settledHistory, data: '<iframe src="about:blank"></iframe>' }, false],
        ['risu trigger markup', { ...settledHistory, data: '<button risu-trigger="run">Run</button>' }, false],
        ['risu button markup', { ...settledHistory, data: '<div risu-btn="run"></div>' }, false],
        ['risu control markup', { ...settledHistory, data: '<div risu-ctrl="bgm"></div>' }, false],
        ['live data markup', { ...settledHistory, data: '<div data-risu-live="true"></div>' }, false],
        ['capture mode', { ...settledHistory, captureAll: true }, false],
    ])('does not contain %s when it must remain live', (_name, input, expected) => {
        expect(shouldContainChatMessage(input)).toBe(expected)
    })
})
