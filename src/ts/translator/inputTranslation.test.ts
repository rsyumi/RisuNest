import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const f = vi.hoisted(() => ({ fetch: vi.fn(), request: vi.fn(), alert: vi.fn(), translatorType: 'deepl', translator: 'ko', deeplXUrl: 'https://deeplx-a.invalid', experimentalGoogle: false, tauri: false }))
vi.mock('../storage/database.svelte', () => ({
    getDatabase: () => ({
        characters: [], presetRegex: [], translator: f.translator, translatorInputLanguage: 'en', aiModel: 'gpt',
        translatorType: f.translatorType, deeplOptions: { freeApi: true, key: 'synthetic' },
        deeplXOptions: { url: f.deeplXUrl, token: '' }, noWaitForTranslate: true,
        useExperimentalGoogleTranslator: f.experimentalGoogle,
    }) as ReturnType<typeof import('../storage/database.svelte').getDatabase>,
    getCurrentCharacter: () => undefined,
    getCurrentChat: () => undefined,
} satisfies Partial<typeof import('../storage/database.svelte')>))
vi.mock('../globalApi.svelte', async importOriginal => ({ ...await importOriginal<typeof import('../globalApi.svelte')>(), globalFetch: f.fetch }))
vi.mock('../process/request/request', async importOriginal => ({ ...await importOriginal<typeof import('../process/request/request')>(), requestChatData: f.request }))
vi.mock('../alert', async importOriginal => ({ ...await importOriginal<typeof import('../alert')>(), alertError: f.alert }))
vi.mock('localforage', async () => (await import('../plugins/localforage.testSupport')).localforageModule(new Map()))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => (
    f.tauri ? { runtime: 'tauri', os: 'ios' } : { runtime: 'web', os: 'windows' }
)))
vi.mock('@tauri-apps/plugin-http', async () => (await import('../globalApi.testSupport')).tauriHttpModule())

import { getDatabase } from '../storage/database.svelte'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import { doingChat } from '../process/generationState'
import { translate, translateHTML, type TranslateHTMLContext } from './translator'

function liveRowContext(): TranslateHTMLContext {
    return {
        scriptContext: {
            presetRegex: [], moduleRegexScripts: [], moduleAssets: [],
            dynamicAssets: false, dynamicAssetsEditDisplay: false,
            parserContext: { database: getDatabase(), character: { type: 'simple', customscript: [], chaId: 'live' } },
        },
        liveProjection: true,
    } as unknown as TranslateHTMLContext
}

beforeEach(() => {
    vi.clearAllMocks()
    f.translatorType = 'deepl'
    f.translator = 'ko'
    f.deeplXUrl = 'https://deeplx-a.invalid'
    f.experimentalGoogle = false
    f.tauri = false
    f.fetch.mockResolvedValue({ ok: false, data: 'synthetic unavailable' })
    f.request.mockResolvedValue({ type: 'fail', result: 'synthetic unavailable' })
})

afterEach(() => {
    doingChat.set(false)
    vi.unstubAllGlobals()
})

describe('translation error ownership', () => {
    it.each(['deepl', 'llm'])('leaves repeated %s failures to the input owner and never caches a failed input as translated', async provider => {
        f.translatorType = provider
        const draft = `${provider} input draft`
        for (let attempt = 0; attempt < 2; attempt++) {
            await expect(translate(draft, false, undefined, 'throw')).rejects.toThrow('synthetic unavailable')
        }
        expect(f.alert).not.toHaveBeenCalled()
        f.fetch.mockResolvedValue({ ok: true, data: { translations: [{ text: 'Translated draft' }] } })
        f.request.mockResolvedValue({ type: 'success', result: 'Translated draft' })
        await expect(translate(draft, false, undefined, 'throw')).resolves.toBe('Translated draft')
    })
    it.each(['deepl', 'llm'])('keeps the %s alert and original text for callers using the default mode and translates again later', async provider => {
        f.translatorType = provider
        const draft = `${provider} explicit translation`
        const row = `<p>${provider} shown row</p><risu-style>.row { color: red; }</risu-style>`
        await expect(translate(draft, false)).resolves.toBe(draft)
        await expect(translateHTML(row, false, '', 0, false, liveRowContext())).resolves.toBe(row)
        const failure = provider === 'deepl' ? 'ERR::DeepL API Errorsynthetic unavailable' : 'ERR::synthetic unavailable'
        expect(new Set(f.alert.mock.calls.map(([message]) => message))).toEqual(new Set([failure]))
        f.fetch.mockResolvedValue({ ok: true, data: { translations: [{ text: 'Translated draft' }] } })
        f.request.mockResolvedValue({ type: 'success', result: 'Translated draft' })
        await expect(translate(draft, false)).resolves.toBe('Translated draft')
    })
    it.each(['deepl', 'llm'])('still rejects a %s input translation that overlaps a pending display translation', async provider => {
        f.translatorType = provider
        const text = `${provider} text shown and typed at once`
        const [display, input] = await Promise.allSettled([translate(text, false, liveRowContext()), translate(text, false, undefined, 'throw')])
        expect(display).toEqual({ status: 'fulfilled', value: text })
        expect(input.status).toBe('rejected')
    })
    it.each(['streaming', 'multiline'])('leaves unexpected %s responses to the input owner', async type => {
        f.translatorType = 'llm'
        f.request.mockResolvedValue({ type, result: [] })
        await expect(translate(`${type} input`, false, undefined, 'throw')).rejects.toThrow('Unexpected response type')
        expect(f.alert).not.toHaveBeenCalled()
    })
})

describe('live chat rows', () => {
    it('reuses a translation made for the same live row text', async () => {
        f.fetch.mockResolvedValue({ ok: true, data: { translations: [{ text: 'Live translated' }] } })
        const text = 'live row text that renders again'
        await expect(translate(text, false, liveRowContext())).resolves.toBe('Live translated')
        await expect(translate(text, false, liveRowContext())).resolves.toBe('Live translated')
        expect(f.fetch).toHaveBeenCalledOnce()
    })
    it('sends one request for overlapping translations of the same live row text', async () => {
        f.fetch.mockResolvedValue({ ok: true, data: { translations: [{ text: 'Shared translated' }] } })
        const text = 'live row text parsed twice at once'
        await expect(Promise.all([translate(text, false, liveRowContext()), translate(text, false, liveRowContext())]))
            .resolves.toEqual(['Shared translated', 'Shared translated'])
        expect(f.fetch).toHaveBeenCalledOnce()
    })
    it('sends one LLM request for overlapping translations of the same live row', async () => {
        f.translatorType = 'llm'
        f.request.mockResolvedValue({ type: 'success', result: '<p>LLM shared</p>' })
        const html = '<p>live LLM row parsed twice at once</p>'
        await expect(Promise.all([
            translateHTML(html, false, '', 0, false, liveRowContext()),
            translateHTML(html, false, '', 0, false, liveRowContext()),
        ])).resolves.toEqual(['<p>LLM shared</p>', '<p>LLM shared</p>'])
        expect(f.request).toHaveBeenCalledOnce()
    })
    it('leaves a live row untranslated while a reply is generated', async () => {
        f.fetch.mockResolvedValue({ ok: true, data: { translations: [{ text: 'Streaming translated' }] } })
        doingChat.set(true)
        await expect(translateHTML('<p>streaming reply</p>', false, '', 0, false, liveRowContext())).resolves.toBe('<p>streaming reply</p>')
        expect(f.fetch).not.toHaveBeenCalled()
    })
    it('keeps the translations made earlier for a live row while a reply is generated', async () => {
        f.fetch.mockImplementation(async (_url: string, init: { body: { text: string[] } }) => (
            { ok: true, data: { translations: [{ text: `KO ${init.body.text[0]}` }] } }
        ))
        await expect(translateHTML('<p>earlier reply</p>', false, '', 0, false, liveRowContext())).resolves.toBe('<p>KO earlier reply</p>')
        doingChat.set(true)
        await expect(translateHTML('<p>earlier reply</p>', false, '', 0, false, liveRowContext())).resolves.toBe('<p>KO earlier reply</p>')
        await expect(translateHTML('<p>earlier reply</p><p>newer reply</p>', false, '', 0, false, liveRowContext()))
            .resolves.toBe('<p>earlier reply</p><p>newer reply</p>')
        expect(f.fetch).toHaveBeenCalledOnce()
    })
    it('translates the same text again for another target language', async () => {
        f.fetch.mockImplementation(async (_url: string, init: { body: { target_lang: string } }) => (
            { ok: true, data: { translations: [{ text: `in ${init.body.target_lang}` }] } }
        ))
        const text = 'text read in two languages'
        await expect(translate(text, false)).resolves.toBe('in KO')
        await expect(translate('in KO', true)).resolves.toBe(text)
        f.translator = 'ja'
        await expect(translate(text, false)).resolves.toBe('in JA')
        expect(f.fetch).toHaveBeenCalledTimes(2)
    })
    it('translates a live row again after the DeepLX server changes', async () => {
        f.translatorType = 'deeplX'
        f.fetch.mockImplementation(async (url: string) => (
            { ok: true, data: { data: url.startsWith('https://deeplx-a.invalid') ? 'from server A' : 'from server B' } }
        ))
        await expect(translateHTML('<p>live row text</p>', false, '', 0, false, liveRowContext())).resolves.toBe('<p>from server A</p>')
        f.deeplXUrl = 'https://deeplx-b.invalid'
        await expect(translateHTML('<p>live row text</p>', false, '', 0, false, liveRowContext())).resolves.toBe('<p>from server B</p>')
    })
    it('translates a live row again after experimental Google translation is enabled', async () => {
        f.translatorType = 'google'
        vi.stubGlobal('userScriptFetch', true)
        vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify([[['standard Google']]]))))
        f.fetch.mockResolvedValue({ ok: true, data: new TextEncoder().encode('<div class="result-container">experimental Google</div>') })
        const html = '<p>Google setting cache fixture</p>'
        await expect(translateHTML(html, false, '', 0, false, liveRowContext())).resolves.toBe('<p>standard Google</p>')
        f.experimentalGoogle = true
        await expect(translateHTML(html, false, '', 0, false, liveRowContext())).resolves.toBe('<p>experimental Google</p>')
        f.experimentalGoogle = false
        await expect(translateHTML(html, false, '', 0, false, liveRowContext())).resolves.toBe('<p>standard Google</p>')
    })
    it('keeps translations within the memory budget of the device profile', async () => {
        f.fetch.mockResolvedValue({ ok: true, data: { translations: [{ text: 'Long translated' }] } })
        setRuntimePerformanceProfile('low-spec')
        try {
            const text = 'x'.repeat(3 * 1024 * 1024)
            await translate(text, false)
            await translate(text, false)
        } finally {
            setRuntimePerformanceProfile('normal')
        }
        expect(f.fetch).toHaveBeenCalledTimes(2)
    })
})

describe('Google Translate', () => {
    type SentRequest = { method?: string, body?: string, dataText?: string }
    const translated = () => new Response(JSON.stringify([[['KO paragraph']]]))
    const refused = () => new Response('<html>synthetic refusal</html>', { status: 429 })
    const unreachable = (): Response => { throw new TypeError('Load failed') }
    function googleRequest(runtime: 'tauri' | 'web', webView: (url: string, q: string | null) => Response | Promise<Response>, native: (url: string) => Response = translated) {
        f.translatorType = 'google'
        f.tauri = runtime === 'tauri'
        const webFetch = vi.fn(async (url: URL | string, init: SentRequest) => webView(String(url), new URLSearchParams(init.body).get('q')))
        vi.stubGlobal('fetch', webFetch)
        vi.mocked(pluginFetch).mockImplementation(async url => native(String(url)))
        const requests = (calls: unknown[][], body: 'body' | 'dataText') => calls.map(([url, init]) => (
            { url: String(url), method: (init as SentRequest).method, q: new URLSearchParams((init as SentRequest)[body]).get('q') }
        ))
        return () => ({ webView: requests(webFetch.mock.calls, 'body'), native: requests(vi.mocked(pluginFetch).mock.calls, 'dataText') })
    }
    const endpoint = 'https://translate.googleapis.com/translate_a/single?client=dict-chrome-ex&dt=t&sl=en&tl=ko'
    const clientOf = ({ url }: { url: string }) => new URL(url).searchParams.get('client')
    // Translations remember the client that answered, so every test starts the translator anew.
    let translate: typeof import('./translator').translate
    let pluginFetch: typeof import('@tauri-apps/plugin-http').fetch
    beforeEach(async () => {
        vi.resetModules()
        ;({ translate } = await import('./translator'))
        ;({ fetch: pluginFetch } = await import('@tauri-apps/plugin-http'))
    })

    it.each(['tauri', 'web'] as const)('sends a paragraph of any length in the WebView request body on %s', async runtime => {
        const sent = googleRequest(runtime, translated)
        const text = `${runtime} ` + 'A paragraph longer than a URL can carry. '.repeat(500).trim()
        await expect(translate(text, false)).resolves.toBe('KO paragraph')
        expect(sent()).toEqual({ webView: [{ url: endpoint, method: 'POST', q: text }], native: [] })
    })
    it.each([['a network error', false], ['a 429', true]])('asks the native client on Tauri and releases the WebView response when the WebView request ends in %s', async (failure, refusedByGoogle) => {
        const release = vi.fn()
        const sent = googleRequest('tauri', () => refusedByGoogle ? new Response(new ReadableStream({ cancel: release }), { status: 429 }) : unreachable())
        const text = `paragraph the WebView cannot get translated after ${failure}`
        await expect(translate(text, false)).resolves.toBe('KO paragraph')
        expect(sent().native).toEqual([{ url: endpoint, method: 'POST', q: text }])
        expect(release).toHaveBeenCalledTimes(refusedByGoogle ? 1 : 0)
    })
    it('translates through the experimental Google page on Tauri', async () => {
        const { globalFetch } = await vi.importActual<typeof import('../globalApi.svelte')>('../globalApi.svelte')
        const page = '<html><body><div class="result-container">KO experimental</div></body></html>'
        googleRequest('tauri', translated, () => new Response(page, { headers: { 'Content-Type': 'text/html' } }))
        f.experimentalGoogle = true
        f.fetch.mockImplementation(globalFetch)
        await expect(translate('paragraph for the experimental Google page', false)).resolves.toBe('KO experimental')
    })
    it('passes on the native error when neither client reaches Google on Tauri', async () => {
        googleRequest('tauri', unreachable, () => { throw new TypeError('synthetic native transport failure') })
        await expect(translate('paragraph neither client can send', false)).rejects.toThrow('synthetic native transport failure')
    })
    it('keeps the original text, reports the status and releases every native response when Google refuses every client on Tauri', async () => {
        const release = vi.fn()
        const sent = googleRequest('tauri', unreachable, () => new Response(new ReadableStream({ cancel: release }), { status: 429 }))
        const text = 'paragraph Google refuses'
        await expect(translate(text, false)).resolves.toBe(text)
        expect(f.alert).toHaveBeenCalledExactlyOnceWith('ERR::Google Translate Error 429')
        expect(sent().native.map(clientOf)).toEqual(['dict-chrome-ex', 'at', 'gtx'])
        expect(release).toHaveBeenCalledTimes(3)
    })
    it.each([['tauri', refused], ['web', unreachable]] as const)('asks the next client when Google refuses one on %s and keeps asking the client that answered', async (runtime, refusal) => {
        let refusedClient = 'dict-chrome-ex'
        const answer = (url: string) => clientOf({ url }) === refusedClient ? refusal() : translated()
        const sent = googleRequest(runtime, answer, answer)
        await expect(translate(`${runtime} paragraph after the first client is refused`, false)).resolves.toBe('KO paragraph')
        await expect(translate(`${runtime} paragraph for the client that answered`, false)).resolves.toBe('KO paragraph')
        refusedClient = 'at'
        await expect(translate(`${runtime} paragraph after the second client is refused`, false)).resolves.toBe('KO paragraph')
        refusedClient = 'gtx'
        await expect(translate(`${runtime} paragraph after the last client is refused`, false)).resolves.toBe('KO paragraph')
        expect(sent().webView.map(clientOf)).toEqual(['dict-chrome-ex', 'at', 'at', 'at', 'gtx', 'gtx', 'dict-chrome-ex'])
        expect(f.alert).not.toHaveBeenCalled()
    })
    it('asks every client for a translation that waited while another one found the client Google answers', async () => {
        let release!: () => void
        const held = new Promise<void>(resolve => { release = resolve })
        const waitingText = 'paragraph whose first request waits'
        const sent = googleRequest('web', async (url, q) => {
            if(q === waitingText && clientOf({ url }) === 'dict-chrome-ex') await held
            return clientOf({ url }) === 'gtx' ? translated() : refused()
        })
        const waiting = translate(waitingText, false)
        await expect(translate('paragraph that finds the client Google answers', false)).resolves.toBe('KO paragraph')
        expect(sent().webView.filter(({ q }) => q === waitingText).map(clientOf)).toEqual(['dict-chrome-ex'])
        release()
        await expect(waiting).resolves.toBe('KO paragraph')
        expect(f.alert).not.toHaveBeenCalled()
    })
    it('asks the next client on Tauri when the native request for a refused client fails', async () => {
        googleRequest('tauri', url => clientOf({ url }) === 'dict-chrome-ex' ? refused() : translated(), () => { throw new TypeError('synthetic native transport failure') })
        await expect(translate('paragraph whose native request fails', false)).resolves.toBe('KO paragraph')
    })
})
