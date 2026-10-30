// @vitest-environment happy-dom

import { afterEach, beforeAll, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'

const f = vi.hoisted(() => ({
    fetch: vi.fn(),
    parse: vi.fn(async (value: string) => value),
}))

vi.mock('../../ts/globalApi.svelte', async (importOriginal) => ({
    ...await importOriginal<typeof import('../../ts/globalApi.svelte')>(),
    globalFetch: f.fetch,
}))
vi.mock('localforage', async () => (await import('src/ts/plugins/localforage.testSupport')).localforageModule(new Map()))
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule({
    ParseMarkdown: f.parse,
    addMetadataToElement: (value: string) => value,
    postTranslationParse: (value: string) => value,
    trimMarkdown: (value: string) => value,
    getDistance: () => 0,
}))

let doingChat: typeof import('src/ts/process/generationState').doingChat
let DBState: typeof import('src/ts/stores.svelte').DBState
let alertStore: typeof import('src/ts/stores.svelte').alertStore
let ChatBody: typeof import('./ChatBody.svelte').default

beforeAll(async () => {
    // Vite evaluates cyclic imports asynchronously. Flush root effects only once
    // the graph is initialized, as in the synchronously bundled application.
    vi.useFakeTimers({ toFake: ['queueMicrotask'] })
    ;({ doingChat } = await import('src/ts/process/generationState'))
    // Loaded before ChatBody so the translator binds the mocked fetch.
    await import('src/ts/translator/translator')
    ;({ DBState, alertStore } = await import('src/ts/stores.svelte'))
    ;({ default: ChatBody } = await import('./ChatBody.svelte'))
    vi.runAllTicks()
    vi.useRealTimers()
})

function liveDatabase() {
    const character = {
        type: 'character', name: 'Live Character', chaId: 'live-character', chatPage: 0,
        chats: [{ message: [], note: '', name: '', localLore: [] }], customscript: [],
    }
    return {
        characters: [character], presetRegex: [], modules: [], enabledModules: [], aiModel: 'gpt',
        translator: 'ko', translatorInputLanguage: 'en', translatorType: 'deepl',
        deeplOptions: { freeApi: true, key: 'synthetic' },
        autoTranslate: true, autoTranslateCachedOnly: false, translateBeforeHTMLFormatting: false,
        legacyTranslation: false, showTranslationLoading: false, newImageHandlingBeta: false,
    }
}

function liveProjection(database: ReturnType<typeof liveDatabase>) {
    const character = database.characters[0]
    return {
        kind: 'bounded' as const,
        characterId: character.chaId, conversationId: 'live-conversation', revision: 1,
        totalMessages: 1, chatID: 0, projectedChatID: 0, historyOffset: 0, messages: [],
        context: {
            presetRegex: [], moduleRegexScripts: [], moduleAssets: [], dynamicAssets: false, dynamicAssetsEditDisplay: false,
            parserContext: {
                database, character, userName: 'User', personaPrompt: '', modules: [], moduleLorebooks: [],
                selectedCharID: 0, chatVariables: {}, globalChatVariables: {}, currentTime: 1, historyOffset: 0,
            },
        },
    }
}

describe('live row translation around a generation', () => {
    let mounted: ReturnType<typeof mount> | undefined
    let target: HTMLDivElement

    beforeEach(() => {
        f.fetch.mockReset().mockImplementation(async (_url: string, init: { body: { text: string[] } }) => (
            { ok: true, data: { translations: [{ text: `KO(${init.body.text[0]})` }] } }
        ))
        f.parse.mockReset().mockImplementation(async (value: string) => value)
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        doingChat.set(false)
        alertStore.set({ type: 'none', msg: '' })
        vi.unstubAllGlobals()
        document.body.replaceChildren()
    })

    function mountLiveRow(message: string, settings: Partial<ReturnType<typeof liveDatabase>> = {}) {
        const database = { ...liveDatabase(), ...settings }
        DBState.db = database as unknown as typeof DBState.db
        const props = $state({
            msgDisplay: message,
            renderRevision: 0,
            translated: false,
            idx: 0,
            role: 'char',
            character: database.characters[0] as never,
            translating: false,
            retranslate: false,
            modelShortName: '',
            parserProjection: liveProjection(database) as never,
        })
        mounted = mount(ChatBody, { target, props })
        return props
    }

    test('translates a streamed reply once, after the generation ends', async () => {
        doingChat.set(true)
        const row = mountLiveRow('A')
        await vi.waitFor(() => expect(target.textContent).toBe('A'))
        for (const snapshot of ['A reply', 'A reply that streams']) {
            row.msgDisplay = snapshot
            await vi.waitFor(() => expect(target.textContent).toBe(snapshot))
        }
        expect(f.fetch).not.toHaveBeenCalled()

        doingChat.set(false)
        await vi.waitFor(() => expect(target.textContent).toBe('KO(A reply that streams)'))
        expect(f.fetch).toHaveBeenCalledOnce()
    })

    test('keeps a translated row as it is when a generation it did not render in ends', async () => {
        mountLiveRow('reply before a generation')
        await vi.waitFor(() => expect(target.textContent).toBe('KO(reply before a generation)'))
        const parses = f.parse.mock.calls.length

        doingChat.set(true)
        doingChat.set(false)
        // A parse starts on the next flush and reaches the parser after one task.
        await tick()
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(target.textContent).toBe('KO(reply before a generation)')
        expect(f.parse).toHaveBeenCalledTimes(parses)
        expect(f.fetch).toHaveBeenCalledOnce()
    })

    test('shows a row untranslated when its translation request fails', async () => {
        const translationRequests = vi.fn(async (_url: string | URL) => { throw new TypeError('Load failed') })
        vi.stubGlobal('fetch', translationRequests)
        f.parse.mockImplementation(async (value: string) => `rendered ${value}`)
        mountLiveRow('reply the translator cannot reach', { translatorType: 'google' })

        await vi.waitFor(() => expect(target.textContent).toBe('rendered reply the translator cannot reach'))
        expect(get(alertStore)).toMatchObject({ type: 'error', msg: 'Error while parsing chat message: Load failed' })
        // The translator may ask another endpoint, but the row never sends a request again.
        const urls = translationRequests.mock.calls.map(([url]) => String(url))
        expect(urls).not.toHaveLength(0)
        expect(new Set(urls).size).toBe(urls.length)
    })
})
