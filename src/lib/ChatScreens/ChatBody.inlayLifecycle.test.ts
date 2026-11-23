// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, settled as domSettled, unmount } from 'svelte'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetMetadata: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
}))

const parserMocks = vi.hoisted(() => ({
    ParseMarkdown: vi.fn(),
    addMetadataToElement: vi.fn((value: string) => value),
    postTranslationParse: vi.fn((value: string) => value),
    trimMarkdown: vi.fn((value: string) => value),
    getDistance: vi.fn(() => 0),
}))

const chatState = vi.hoisted(() => ({ db: {} as Record<string, unknown> }))
const liveRenderMocks = vi.hoisted(() => ({
    getModuleAssets: vi.fn(() => []),
    getCurrentCharacter: vi.fn(() => ({})),
    getFileSrc: vi.fn(async (source: string) => `asset://${source}`),
    getFileImageSource: vi.fn(async (): Promise<any> => null),
}))

const schedulingMocks = vi.hoisted(() => {
    const pending: Array<() => void> = []
    const state = { controlled: false }
    return {
        state,
        pending,
        yieldToMainThread: vi.fn(() => {
            if (!state.controlled) return Promise.resolve()
            return new Promise<void>((resolve) => pending.push(resolve))
        }),
        releaseNext() {
            pending.shift()?.()
        },
        releaseAll() {
            for (const resolve of pending.splice(0)) resolve()
        },
    }
})

vi.mock('src/ts/process/files/inlays', () => inlayMocks)
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule(parserMocks))
vi.mock('src/ts/stores.svelte', () => ({ DBState: chatState }))
vi.mock('src/ts/util', () => ({ sleep: vi.fn(() => Promise.resolve()) }))
vi.mock('src/ts/translator/translator', () => ({
    getLLMCache: vi.fn(),
    translateHTML: vi.fn(),
}))
vi.mock('src/ts/process/modules', () => ({ getModuleAssets: liveRenderMocks.getModuleAssets }))
vi.mock('src/ts/storage/database.svelte', () => ({ getCurrentCharacter: liveRenderMocks.getCurrentCharacter }))
vi.mock('src/ts/globalApi.svelte', () => ({ getFileSrc: liveRenderMocks.getFileSrc, getFileImageSource: liveRenderMocks.getFileImageSource }))
vi.mock('src/ts/alert', () => ({ alertError: vi.fn() }))
vi.mock('src/ts/ui/yieldToUi', () => ({
    yieldToMainThread: schedulingMocks.yieldToMainThread,
}))

import {
    mountDeferredInlaySources,
    renderDeferredInlaySourceMarkup,
    type DeferredInlayMarkerRegistry,
} from 'src/ts/process/files/inlayRenderSource'
import ChatBodyInlayHarness from './ChatBodyInlayHarness.test.svelte'
import { displayRowsToRefresh, markDisplayRowsForRebuild, noteChatVariableRead, registerDisplayRow } from 'src/ts/process/scriptVariableReads'
import type { ParseMarkdownRenderContext } from 'src/ts/parser/parser.svelte'
import { maxTextUnitLength } from 'src/ts/parser/boundedTextUnits'

const imageSource = {
    url: '',
    mime: 'image/png',
    type: 'image' as const,
    name: 'image.png',
    size: 1,
    objectUrl: false,
}

class TestIntersectionObserver {
    static instance: TestIntersectionObserver | undefined
    constructor(private readonly callback: IntersectionObserverCallback) {
        TestIntersectionObserver.instance = this
    }
    observe = vi.fn()
    unobserve = vi.fn()
    disconnect = vi.fn()
    takeRecords = () => []
    readonly root = null
    readonly rootMargin = '0px'
    readonly thresholds = [0]
    setVisible(element: Element, visible: boolean) {
        this.callback([{
            target: element,
            isIntersecting: visible,
            intersectionRatio: visible ? 1 : 0,
        } as IntersectionObserverEntry], this as unknown as IntersectionObserver)
    }
}

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((done) => { resolve = done })
    return { promise, resolve }
}

function minimalCaptureContext() {
    const character = {
        type: 'character' as const,
        name: 'Frozen Character',
        chaId: 'frozen-character',
        chatPage: 0,
        chats: [{ message: [], note: '', name: '', localLore: [] }],
        customscript: [],
    }
    return {
        character: null,
        characterName: 'Frozen Character',
        characterImageSource: '',
        characterLargePortrait: false,
        userName: 'Frozen User',
        userImageSource: '',
        userLargePortrait: false,
        moduleAssets: [],
        presetRegex: [],
        moduleRegexScripts: [],
        assetStyle: '',
        parserContext: {
            database: { characters: [character] },
            character,
            userName: 'Frozen User',
            personaPrompt: '',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
        },
        settings: {
            autoTranslate: false,
            autoTranslateCachedOnly: false,
            translatorType: 'mock',
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
    } as any
}

describe('ChatBody deferred inlay lifecycle', () => {
    let mounted: ReturnType<typeof mount> | undefined
    let target: HTMLDivElement
    let createObjectURL: ReturnType<typeof vi.fn>
    let revokeObjectURL: ReturnType<typeof vi.fn>

    beforeEach(() => {
        vi.clearAllMocks()
        parserMocks.trimMarkdown.mockImplementation((value: string) => value)
        vi.stubGlobal('IntersectionObserver', undefined)
        chatState.db = {}
        liveRenderMocks.getFileImageSource.mockReset().mockResolvedValue(null)
        liveRenderMocks.getModuleAssets.mockReturnValue([])
        schedulingMocks.state.controlled = false
        schedulingMocks.releaseAll()
        schedulingMocks.yieldToMainThread.mockClear()
        target = document.createElement('div')
        document.body.append(target)
        createObjectURL = vi.fn((blob: Blob) => blob.size === 5 ? 'blob:first' : 'blob:second')
        revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL })
        parserMocks.ParseMarkdown.mockImplementation(async (message: string, ...args: unknown[]) => {
            const context = args[4] as { deferredInlays?: DeferredInlayMarkerRegistry } | undefined
            return renderDeferredInlaySourceMarkup(message, imageSource, context?.deferredInlays)
        })
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        schedulingMocks.state.controlled = false
        schedulingMocks.releaseAll()
        await Promise.resolve()
        document.body.replaceChildren()
        vi.unstubAllGlobals()
        vi.useRealTimers()
    })

    test('reports ordinary body readiness without waiting for image bytes or capture completion', async () => {
        const bytes = deferred<any>()
        inlayMocks.getInlayAssetBlob.mockReturnValue(bytes.promise)
        const settled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onDisplaySettled: settled } })
        await vi.waitFor(() => expect(settled).toHaveBeenCalledOnce())
        expect(target.querySelector('img')).not.toBeNull()
        expect(target.querySelector('img')?.hasAttribute('src')).toBe(false)
        bytes.resolve({ data: new Blob(['first']), type: 'image', name: 'synthetic.png' })
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:first'))
        expect(settled).toHaveBeenCalledOnce()
        // Plugins and custom CSS select the rendered markup as direct children of the text root.
        expect(target.querySelector('img')?.parentElement).toBe(target.firstElementChild)
    })

    test('learns beta image dimensions after identical markup reuses the displayed DOM', async () => {
        const recordDimensions = vi.fn(async () => {})
        chatState.db.newImageHandlingBeta = true
        liveRenderMocks.getModuleAssets.mockReturnValue([['synthetic.png', 'assets/synthetic.png', 'png']] as never)
        liveRenderMocks.getFileImageSource.mockResolvedValue({
            url: 'https://synthetic.invalid/image.png', recordDimensions, contentHash: 'a'.repeat(64),
            metadata: { kind: 'asset', key: 'assets/synthetic.png', mime: 'image/png', name: 'synthetic.png', size: 1 },
        })
        parserMocks.ParseMarkdown.mockResolvedValue('<img src="synthetic.png">')
        const onDisplaySettled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onDisplaySettled } })
        await vi.waitFor(() => expect(onDisplaySettled).toHaveBeenCalledOnce())
        const image = target.querySelector('img')!
        Object.defineProperties(image, { naturalWidth: { value: 320 }, naturalHeight: { value: 200 } })
        ;(mounted as { reload(): void }).reload()
        await vi.waitFor(() => expect(onDisplaySettled).toHaveBeenCalledTimes(2))
        expect(target.querySelector('img')).toBe(image)
        image.dispatchEvent(new Event('load'))
        expect(recordDimensions).toHaveBeenCalledExactlyOnceWith(320, 200)
        liveRenderMocks.getModuleAssets.mockReturnValue([])
    })

    test('prepares beta image dimensions before publishing the first DOM', async () => {
        const metadata = deferred<any>()
        chatState.db.newImageHandlingBeta = true
        liveRenderMocks.getModuleAssets.mockReturnValue([['synthetic.png', 'assets/synthetic.png', 'png']] as never)
        liveRenderMocks.getFileImageSource.mockReturnValue(metadata.promise)
        parserMocks.ParseMarkdown.mockResolvedValue('<img src="synthetic.png">')
        const settled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onDisplaySettled: settled } })
        await vi.waitFor(() => expect(liveRenderMocks.getFileImageSource).toHaveBeenCalled())
        expect(target.querySelector('img')).toBeNull()
        metadata.resolve({
            url: 'https://synthetic.invalid/image.png', contentHash: 'a'.repeat(64), width: 640, height: 480,
            metadata: { kind: 'asset', key: 'assets/synthetic.png', mime: 'image/png', name: 'synthetic.png', size: 1 },
        })
        await vi.waitFor(() => expect(settled).toHaveBeenCalledOnce())
        expect(target.querySelector('img')?.getAttribute('width')).toBe('640')
        expect(target.querySelector('img')?.getAttribute('height')).toBe('480')
    })

    test('does not publish stale beta image preparation after a message changes', async () => {
        const metadata = deferred<any>()
        chatState.db.newImageHandlingBeta = true
        liveRenderMocks.getModuleAssets.mockReturnValue([['synthetic.png', 'assets/synthetic.png', 'png']] as never)
        liveRenderMocks.getFileImageSource.mockReturnValue(metadata.promise)
        parserMocks.ParseMarkdown.mockImplementation(async message => message === 'first' ? '<img src="synthetic.png">' : '<p>second</p>')
        const settled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onDisplaySettled: settled } })
        await vi.waitFor(() => expect(liveRenderMocks.getFileImageSource).toHaveBeenCalled())
        ;(mounted as { setMessage(value: string): void }).setMessage('second')
        await vi.waitFor(() => expect(target.textContent).toContain('second'))
        metadata.resolve({
            url: 'https://synthetic.invalid/image.png', contentHash: 'a'.repeat(64), width: 640, height: 480,
            metadata: { kind: 'asset', key: 'assets/synthetic.png', mime: 'image/png', name: 'synthetic.png', size: 1 },
        })
        await domSettled()
        await domSettled()
        expect(target.querySelector('img')).toBeNull()
        expect(target.textContent).toContain('second')
        expect(settled).toHaveBeenCalledOnce()
    })

    test('keeps message text visible if a beta image cannot be resolved', async () => {
        chatState.db.newImageHandlingBeta = true
        liveRenderMocks.getModuleAssets.mockReturnValue([['synthetic.png', 'assets/synthetic.png', 'png']] as never)
        liveRenderMocks.getFileImageSource.mockRejectedValue(new Error('synthetic missing image'))
        parserMocks.ParseMarkdown.mockResolvedValue('<p>Readable message</p><img src="synthetic.png">')
        const settled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onDisplaySettled: settled } })
        await vi.waitFor(() => expect(settled).toHaveBeenCalledOnce())
        expect(target.textContent).toContain('Readable message')
    })

    test('does not resolve hidden images through the beta asset-name lookup', async () => {
        chatState.db.newImageHandlingBeta = true
        chatState.db.hideAllImages = true
        parserMocks.ParseMarkdown.mockResolvedValue('<img src="/none.webp" alt="?">')
        const settled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onDisplaySettled: settled } })
        await vi.waitFor(() => expect(settled).toHaveBeenCalledOnce())
        expect(liveRenderMocks.getFileImageSource).not.toHaveBeenCalled()
        expect(target.querySelector('img')?.getAttribute('src')).toBe('/none.webp')
    })

    test('mounts a deferred collapsed thought body only when opened, retaining its state across updates', async () => {
        const body = 'FULL REASONING '.repeat(20_000)
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                initialMessage: `<Thoughts>${body}</Thoughts>Answer`,
                initialThoughtPreview: true,
                streamingThoughtMode: 'collapsed',
            },
        })
        await domSettled()
        const details = target.querySelector('details')!
        expect(details.open).toBe(false)
        expect(target.textContent!.length).toBeLessThan(100)
        details.open = true
        details.dispatchEvent(new Event('toggle'))
        await domSettled()
        // The large body is mounted over animation frames.
        expect(details.textContent).toContain(body.slice(0, 1024))
        await vi.waitFor(() => expect(details.textContent).toContain(body))
        ;(mounted as { setMessage(value: string): void }).setMessage(
            `<Thoughts>${body}LATEST</Thoughts>Answer`,
        )
        await domSettled()
        expect(target.querySelector('details')).toBe(details)
        expect(details.open).toBe(true)
        expect(details.textContent).toContain('LATEST')
        expect(parserMocks.ParseMarkdown).not.toHaveBeenCalled()
    })

    test('preserves a live collapsed block that the user expanded through parsed replacements', async () => {
        parserMocks.ParseMarkdown.mockImplementation(
            async (value, _char, _mode, _idx, _conditions, context) => {
                expect(context?.streamingThoughtMode).toBe('collapsed')
                return `<details data-risu-thought data-risu-streaming-thought><summary>Thought</summary>${value}</details>`
            },
        )
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { streamingThoughtMode: 'collapsed' },
        })
        await vi.waitFor(() =>
            expect(target.querySelector('details')).not.toBeNull(),
        )
        target.querySelector('details')!.open = true
        ;(mounted as { setMessage(value: string): void }).setMessage('new content')
        await vi.waitFor(() =>
            expect(target.querySelector('details')?.textContent).toContain(
                'new content',
            ),
        )
        expect(target.querySelector('details')?.open).toBe(true)
    })

    test('keeps a large expanded live block open and refills it in bounded units after parsed replacements', async () => {
        const body = 'synthetic '.repeat(maxTextUnitLength / 2)
        parserMocks.ParseMarkdown.mockImplementation(async (value) =>
            `<details data-risu-thought data-risu-streaming-thought><summary>Thought</summary>${value}</details>`)
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { initialMessage: `${body}FIRST`, streamingThoughtMode: 'collapsed' },
        })
        await vi.waitFor(() => expect(target.querySelector('details')).not.toBeNull())
        const first = target.querySelector('details')!
        first.open = true
        first.dispatchEvent(new Event('toggle'))
        await vi.waitFor(() => expect(first.textContent).toContain('FIRST'))
        ;(mounted as { setMessage(value: string): void }).setMessage(`${body}UPDATED`)
        await vi.waitFor(() => expect(target.querySelector('details')).not.toBe(first))
        const updated = target.querySelector<HTMLDetailsElement>('details')!
        expect(updated.open).toBe(true)
        await vi.waitFor(() => expect(updated.textContent).toContain(`${body}UPDATED`))
        expect(updated.open).toBe(true)
        for (const unit of updated.querySelectorAll('[data-risu-thought-unit]')) {
            expect(unit.textContent!.length).toBeLessThanOrEqual(maxTextUnitLength)
        }
    })

    test('settles an expanded preview of a large thought collapsed', async () => {
        const body = 'synthetic '.repeat(maxTextUnitLength / 2)
        parserMocks.ParseMarkdown.mockResolvedValue(`<details data-risu-thought><summary>Thought</summary>${body}</details>`)
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { initialMessage: `<Thoughts>${body}</Thoughts>`, initialThoughtPreview: true, streamingThoughtMode: 'collapsed' },
        })
        await domSettled()
        const preview = target.querySelector('details')!
        preview.open = true
        preview.dispatchEvent(new Event('toggle'))
        await domSettled()
        ;(mounted as { setThoughtPreview(value: boolean): void }).setThoughtPreview(false)
        ;(mounted as { setThoughtMode(value: string): void }).setThoughtMode('off')
        await vi.waitFor(() => {
            const element = target.querySelector('details[data-risu-thought]')
            expect(element?.textContent).toBe('Thought')
            expect(element).not.toBe(preview)
        })
        await domSettled()
        const settled = target.querySelector<HTMLDetailsElement>('details[data-risu-thought]')!
        expect(settled.open).toBe(false)
        expect(settled.querySelectorAll('[data-risu-thought-unit]')).toHaveLength(0)
    })

    test.each([true, false])('retains expanded thoughts when preview=%s settles to canonical markup', async (preview) => {
        parserMocks.ParseMarkdown.mockImplementation(async () => '<details data-risu-thought><summary>Thought</summary>reasoning</details>')
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                initialMessage: '<Thoughts>reasoning</Thoughts>Answer',
                initialThoughtPreview: preview,
                streamingThoughtMode: 'collapsed',
            },
        })
        await vi.waitFor(() => expect(target.querySelector('details')).not.toBeNull())
        const details = target.querySelector('details')!
        details.open = true
        details.dispatchEvent(new Event('toggle'))
        await domSettled()
        ;(mounted as { setThoughtPreview(value: boolean): void }).setThoughtPreview(false)
        ;(mounted as { setThoughtMode(value: string): void }).setThoughtMode('off')
        await vi.waitFor(() => expect(target.querySelector('details[data-risu-thought]')?.textContent).toContain('reasoning'))
        await domSettled()
        expect(target.querySelector<HTMLDetailsElement>('details[data-risu-thought]')!.open).toBe(true)
    })

    test('transfers preview expansion only to the last thought block on settle', async () => {
        parserMocks.ParseMarkdown.mockResolvedValue('<details data-risu-thought><summary>First</summary>first</details><details data-risu-thought><summary>Last</summary>last</details>')
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { initialMessage: '<Thoughts>first</Thoughts><Thoughts>last</Thoughts>', initialThoughtPreview: true, streamingThoughtMode: 'collapsed' },
        })
        await domSettled()
        const preview = target.querySelector('details')!
        preview.open = true
        preview.dispatchEvent(new Event('toggle'))
        await domSettled()
        ;(mounted as { setThoughtPreview(value: boolean): void }).setThoughtPreview(false)
        ;(mounted as { setThoughtMode(value: string): void }).setThoughtMode('off')
        await vi.waitFor(() => expect(target.querySelectorAll('details[data-risu-thought]')).toHaveLength(2))
        expect([...target.querySelectorAll<HTMLDetailsElement>('details[data-risu-thought]')].map((element) => element.open)).toEqual([false, true])
    })

    test('defers display parsing even if translation was enabled and keeps raw text until the final render is ready', async () => {
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { initialTranslated: true, deferStreamingDisplay: true },
        })
        const harness = mounted as {
            setRaw(value: boolean): void
            setTranslated(value: boolean): void
        }
        harness.setRaw(true)
        await domSettled()
        expect(parserMocks.ParseMarkdown).not.toHaveBeenCalled()
        expect(target.textContent).toBe('streaming')
        const gate = deferred<string>()
        parserMocks.ParseMarkdown.mockReturnValue(gate.promise)
        harness.setTranslated(false)
        harness.setRaw(false)
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledOnce(),
        )
        expect(target.textContent).toBe('streaming')
        gate.resolve('<strong>Final</strong>')
        await vi.waitFor(() =>
            expect(target.querySelector('strong')?.textContent).toBe('Final'),
        )
    })

    test('rerenders the same source canonically when only the streaming thought mode ends', async () => {
        parserMocks.ParseMarkdown.mockImplementation(
            async (value, _char, _mode, _idx, _conditions, context) =>
                context?.streamingThoughtMode === 'recent'
                    ? '<span>Recent</span>'
                    : `<strong>${value}</strong>`,
        )
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { initialMessage: 'FULL', streamingThoughtMode: 'recent' },
        })
        await vi.waitFor(() => expect(target.textContent).toBe('Recent'))
        ;(mounted as { setThoughtMode(value: string): void }).setThoughtMode('off')
        await vi.waitFor(() =>
            expect(target.querySelector('strong')?.textContent).toBe('FULL'),
        )
    })

    test('bounds streaming thoughts and applies a deleting regex to the full original after streaming', async () => {
        const original =
            '<Thoughts>' +
            'old reasoning '.repeat(40_000) +
            '\nLATEST</Thoughts>\n**Answer**'
        const finalGate = deferred<string>()
        parserMocks.ParseMarkdown.mockImplementation((value: string) => {
            expect(value).toBe(original)
            return finalGate.promise
        })
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { initialMessage: original, initialThoughtPreview: true },
        })
        await domSettled()
        const preview = target.querySelector('[data-streaming-thought-preview]')
        expect(preview?.textContent).toContain('LATEST')
        expect(preview!.textContent!.length).toBeLessThan(1_000)
        expect(target.textContent).toContain('**Answer**')
        expect(parserMocks.ParseMarkdown).not.toHaveBeenCalled()

        ;(mounted as { setThoughtPreview(value: boolean): void }).setThoughtPreview(
            false,
        )
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledOnce(),
        )
        expect(target.querySelector('[data-streaming-thought-preview]')).toBe(
            preview,
        )
        finalGate.resolve(
            original
                .replace(/<Thoughts>[\s\S]*?<\/Thoughts>/g, '')
                .trim()
                .replace('**Answer**', '<strong>Answer</strong>'),
        )
        await vi.waitFor(() =>
            expect(target.querySelector('strong')?.textContent).toBe('Answer'),
        )
        expect(target.querySelector('[data-streaming-thought-preview]')).toBeNull()
        expect(target.textContent).toBe('Answer')
    })

    test('releases old media when entering a thought preview and discards a cancelled final render', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first']),
            type: 'image',
            name: 'first.png',
        })
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() => expect(target.querySelector('img')).not.toBeNull())
        const harness = mounted as {
            setMessage(value: string): void
            setThoughtPreview(value: boolean): void
        }
        harness.setMessage('<Thoughts>Partial reasoning')
        harness.setThoughtPreview(true)
        await domSettled()
        expect(target.querySelector('img')).toBeNull()
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:first')
        const gate = deferred<string>()
        parserMocks.ParseMarkdown.mockReturnValue(gate.promise)
        harness.setThoughtPreview(false)
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(2),
        )
        await unmount(mounted)
        mounted = undefined
        gate.resolve('<strong>Stale result</strong>')
        await domSettled()
        expect(target.textContent).toBe('')
    })

    test('clears the preview even when the final display rule removes the whole message', async () => {
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                initialMessage: '<Thoughts>Only thoughts</Thoughts>',
                initialThoughtPreview: true,
            },
        })
        await domSettled()
        expect(target.textContent).toContain('Only thoughts')
        parserMocks.ParseMarkdown.mockResolvedValue('')
        ;(mounted as { setThoughtPreview(value: boolean): void }).setThoughtPreview(
            false,
        )
        await vi.waitFor(() => expect(target.textContent).toBe(''))
    })

    test('renders preview text inertly and keeps its block when the provider rewrites the snapshot', async () => {
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                initialMessage: '<Thoughts><img src=x onerror=alert(1)></Thoughts>',
                initialThoughtPreview: true,
            },
        })
        await domSettled()
        const preview = target.querySelector('[data-streaming-thought-preview]')
        expect(target.querySelector('img')).toBeNull()
        expect(preview?.textContent).toContain('<img src=x')
        ;(mounted as { setMessage(value: string): void }).setMessage(
            '<Thoughts>Replaced reasoning</Thoughts>Answer',
        )
        await domSettled()
        expect(target.querySelector('[data-streaming-thought-preview]')).toBe(
            preview,
        )
        expect(preview?.textContent).toContain('Replaced reasoning')
        expect(parserMocks.ParseMarkdown).not.toHaveBeenCalled()
    })

    test('revokes mounted URLs exactly once on parsed rerender, raw switch, and later destroy', async () => {
        inlayMocks.getInlayAssetBlob.mockImplementation(async (id: string) => ({
            data: new Blob([id], { type: 'image/png' }),
            type: 'image',
            name: `${id}.png`,
        }))
        mounted = mount(ChatBodyInlayHarness, { target })

        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:first'))
        ;(mounted as { setMessage(value: string): void }).setMessage('second')
        await domSettled()
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:second'))
        expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:first')).toHaveLength(1)

        ;(mounted as { setRaw(value: boolean): void }).setRaw(true)
        await domSettled()
        expect(target.querySelector('img')).toBeNull()
        expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:second')).toHaveLength(1)

        await unmount(mounted)
        mounted = undefined
        expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:first')).toHaveLength(1)
        expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:second')).toHaveLength(1)
    })

    test('keeps the displayed image and its URL until a reload replacement is ready', async () => {
        inlayMocks.getInlayAssetBlob.mockImplementation(async (id: string) => ({
            data: new Blob([id]),
            type: 'image',
            name: `${id}.png`,
        }))
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() =>
            expect(target.querySelector('img')?.getAttribute('src')).toBe(
                'blob:first',
            ),
        )
        const image = target.querySelector('img')
        const gate = deferred<void>()
        parserMocks.ParseMarkdown.mockImplementationOnce(
            async (_message: string, ...args: unknown[]) => {
                await gate.promise
                return renderDeferredInlaySourceMarkup(
                    'second',
                    imageSource,
                    (args[4] as any).deferredInlays,
                )
            },
        )
        ;(mounted as { reload(): void }).reload()
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(2),
        )
        expect(target.querySelector('img')).toBe(image)
        expect(image?.getAttribute('src')).toBe('blob:first')
        expect(revokeObjectURL).not.toHaveBeenCalled()
        gate.resolve()
        await vi.waitFor(() =>
            expect(target.querySelector('img')?.getAttribute('src')).toBe(
                'blob:second',
            ),
        )
        expect(
            revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:first'),
        ).toHaveLength(1)
    })

    test('keeps identical HTML nodes and inlay leases across a settled reload', async () => {
        const settled = vi.fn()
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first']),
            type: 'image',
            name: 'first.png',
        })
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { onCaptureSettled: settled },
        })
        await vi.waitFor(() => expect(settled).toHaveBeenCalledOnce())
        const image = target.querySelector('img')
        ;(mounted as { reload(): void }).reload()
        await vi.waitFor(() => expect(settled).toHaveBeenCalledTimes(2))
        expect(target.querySelector('img')).toBe(image)
        expect(image?.getAttribute('src')).toBe('blob:first')
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce()
        expect(revokeObjectURL).not.toHaveBeenCalled()
        await unmount(mounted)
        mounted = undefined
        expect(
            revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:first'),
        ).toHaveLength(1)
    })

    test('rebuilds identical HTML and its inlays after a script rebuilds the row, keeping thoughts open', async () => {
        const settled = vi.fn()
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first']),
            type: 'image',
            name: 'first.png',
        })
        parserMocks.ParseMarkdown.mockImplementation(async (message: string, ...args: unknown[]) => {
            const context = args[4] as { deferredInlays?: DeferredInlayMarkerRegistry } | undefined
            return '<details data-risu-thought><summary>Thought</summary>thinking</details><input type="checkbox">' +
                renderDeferredInlaySourceMarkup(message, imageSource, context?.deferredInlays)
        })
        const row = registerDisplayRow(() => 0)
        try {
            mounted = mount(ChatBodyInlayHarness, {
                target,
                props: { onDisplaySettled: settled, displayRow: row },
            })
            await vi.waitFor(() => expect(settled).toHaveBeenCalledOnce())
            const panel = target.querySelector('input')!
            const image = target.querySelector('img')
            panel.checked = true
            target.querySelector('details')!.open = true

            // A refresh for a variable change keeps the open panel.
            ;(mounted as { reload(): void }).reload()
            await vi.waitFor(() => expect(settled).toHaveBeenCalledTimes(2))
            expect(target.querySelector('input')).toBe(panel)

            markDisplayRowsForRebuild(0)
            ;(mounted as { reload(): void }).reload()
            await vi.waitFor(() => expect(settled).toHaveBeenCalledTimes(3))
            expect(target.querySelector('input')).not.toBe(panel)
            expect(target.querySelector('input')?.checked).toBe(false)
            expect(target.querySelector('details')?.open).toBe(true)
            expect(target.querySelector('img')).not.toBe(image)
            expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:first')
            expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(2)
            expect(revokeObjectURL).toHaveBeenCalledExactlyOnceWith('blob:first')
        } finally {
            row.dispose()
        }
    })

    test('retains displayed HTML through superseded parses and ignores their late results', async () => {
        parserMocks.ParseMarkdown.mockResolvedValueOnce('<button>initial</button>')
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() =>
            expect(target.querySelector('button')?.textContent).toBe('initial'),
        )
        const button = target.querySelector('button')
        const stale = deferred<string>()
        const latest = deferred<string>()
        parserMocks.ParseMarkdown.mockReturnValueOnce(
            stale.promise,
        ).mockReturnValueOnce(latest.promise)
        ;(mounted as { reload(): void }).reload()
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(2),
        )
        ;(mounted as { reload(): void }).reload()
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(3),
        )
        expect(target.querySelector('button')).toBe(button)
        latest.resolve('<button>latest</button>')
        await vi.waitFor(() =>
            expect(target.querySelector('button')?.textContent).toBe('latest'),
        )
        stale.resolve('<button>obsolete</button>')
        await domSettled()
        await Promise.resolve()
        expect(target.querySelector('button')?.textContent).toBe('latest')
    })

    test('publishes variable-dependent CSS together with its HTML, even for identical encoded markup', async () => {
        const makeProjection = (color: string) =>
            ({
                context: {
                    ...minimalCaptureContext(),
                    parserContext: {
                        ...minimalCaptureContext().parserContext,
                        chatVariables: { color },
                    },
                },
                projectedChatID: 0,
            }) as any
        const markup =
            '<risu-style>encoded CSS with variable</risu-style><button>panel</button>'
        // CSS variables are decoded by trimMarkdown after the async display parser.
        parserMocks.trimMarkdown.mockImplementation(((
            value: string,
            context: any,
        ) =>
            value.replace(
                '<risu-style>encoded CSS with variable</risu-style>',
                `<style>button { color: ${context.parserContext.chatVariables.color} }</style>`,
            )) as any)
        parserMocks.ParseMarkdown.mockResolvedValue(markup)
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { parserProjection: makeProjection('red') },
        })
        await vi.waitFor(() =>
            expect(target.querySelector('style')?.textContent).toContain('red'),
        )
        const oldStyle = target.querySelector('style')
        const oldButton = target.querySelector('button')
        const pending = deferred<string>()
        parserMocks.ParseMarkdown.mockReturnValueOnce(pending.promise)
        ;(mounted as { setParserProjection(value: any): void }).setParserProjection(
            makeProjection('blue'),
        )
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(2),
        )
        expect(target.querySelector('style')).toBe(oldStyle)
        expect(target.querySelector('button')).toBe(oldButton)
        expect(oldStyle?.textContent).toContain('red')
        pending.resolve(markup)
        await vi.waitFor(() =>
            expect(target.querySelector('style')?.textContent).toContain('blue'),
        )
        expect(target.querySelector('button')?.textContent).toBe('panel')
    })

    test('retains the displayed markup when its projection is aborted before a replacement is admitted', async () => {
        const controller = new AbortController()
        const markup =
            '<style>button { color: red }</style><button>old panel</button>'
        parserMocks.ParseMarkdown.mockResolvedValueOnce(markup)
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { parserAbortSignal: controller.signal },
        })
        await vi.waitFor(() =>
            expect(target.querySelector('button')?.textContent).toBe('old panel'),
        )
        const button = target.querySelector('button')
        const style = target.querySelector('style')
        const pending = deferred<string>()
        parserMocks.ParseMarkdown.mockReturnValueOnce(pending.promise)
        ;(mounted as { setMessage(value: string): void }).setMessage('superseded')
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(2),
        )
        controller.abort()
        pending.resolve('<button>obsolete panel</button>')
        await domSettled()
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(target.querySelector('button')).toBe(button)
        expect(target.querySelector('style')).toBe(style)
        parserMocks.ParseMarkdown.mockResolvedValueOnce(
            '<style>button { color: blue }</style><button>latest panel</button>',
        )
        ;(
            mounted as { setParserAbortSignal(value: AbortSignal): void }
        ).setParserAbortSignal(new AbortController().signal)
        ;(mounted as { setMessage(value: string): void }).setMessage('latest')
        await vi.waitFor(() =>
            expect(target.querySelector('button')?.textContent).toBe(
                'latest panel',
            ),
        )
        expect(target.querySelector('style')?.textContent).toContain('blue')
    })

    test('defers live parsing to a later main-thread task', async () => {
        schedulingMocks.state.controlled = true
        mounted = mount(ChatBodyInlayHarness, { target })
        await domSettled()
        await Promise.resolve()

        expect(schedulingMocks.yieldToMainThread).toHaveBeenCalledOnce()
        expect(parserMocks.ParseMarkdown).not.toHaveBeenCalled()

        schedulingMocks.releaseNext()
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledOnce(),
        )
    })

    test('does not parse a live job replaced while its main-thread yield is pending', async () => {
        schedulingMocks.state.controlled = true
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() =>
            expect(schedulingMocks.yieldToMainThread).toHaveBeenCalledOnce(),
        )

        ;(mounted as { setMessage(value: string): void }).setMessage('second')
        await domSettled()
        await vi.waitFor(() =>
            expect(schedulingMocks.yieldToMainThread).toHaveBeenCalledTimes(2),
        )

        schedulingMocks.state.controlled = false
        schedulingMocks.releaseAll()
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledOnce(),
        )
        expect(parserMocks.ParseMarkdown.mock.calls[0][0]).toBe('second')
    })

    test('keeps capture parsing on the immediate readiness path', async () => {
        schedulingMocks.state.controlled = true
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { captureContext: minimalCaptureContext() },
        })

        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledOnce(),
        )
        expect(schedulingMocks.yieldToMainThread).not.toHaveBeenCalled()
    })

    test('ignores a pending translation after translator configuration changes', async () => {
        chatState.db = { autoTranslate: true, translatorType: 'mock', translateBeforeHTMLFormatting: false }
        const { translateHTML } = await import('src/ts/translator/translator')
        const stale = deferred<string>()
        let staleMarkup = ''
        let parseIndex = 0
        parserMocks.ParseMarkdown.mockImplementation(async (_message: string, ...args: unknown[]) => {
            const context = args[4] as { deferredInlays?: DeferredInlayMarkerRegistry }
            return renderDeferredInlaySourceMarkup(++parseIndex === 1 ? 'first' : 'second', imageSource, context.deferredInlays)
        })
        inlayMocks.getInlayAssetBlob.mockImplementation(async (id: string) => ({
            data: new Blob([id], { type: 'image/png' }), type: 'image', name: `${id}.png`,
        }))
        vi.mocked(translateHTML).mockReset()
            .mockImplementationOnce(async (markup) => { staleMarkup = markup; return stale.promise })
            .mockImplementationOnce(async (markup) => `${markup}<span>latest translation</span>`)
        mounted = mount(ChatBodyInlayHarness, { target, props: { initialTranslated: true } })
        await vi.waitFor(() => expect(translateHTML).toHaveBeenCalledOnce())
        ;(mounted as { setTranslatorType(value: string): void }).setTranslatorType('changed')
        await vi.waitFor(() => expect(target.textContent).toContain('latest translation'))
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:second'))
        stale.resolve(`${staleMarkup}<span>obsolete translation</span>`)
        await domSettled()
        await Promise.resolve()
        expect(target.textContent).toContain('latest translation')
        expect(target.textContent).not.toContain('obsolete translation')
        expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:second')
        expect(inlayMocks.getInlayAssetBlob.mock.calls.map(([id]) => id)).toEqual(['second'])
        expect(revokeObjectURL).not.toHaveBeenCalled()
        await unmount(mounted)
        mounted = undefined
        expect(revokeObjectURL.mock.calls).toEqual([['blob:second']])
    })

    test('refreshes its row for the variables the shown body read, and for any variable once translated', async () => {
        vi.useFakeTimers()
        chatState.db = {
            autoTranslate: false,
            translatorType: 'mock',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            newImageHandlingBeta: false,
        }
        const row = registerDisplayRow(() => 0)
        try {
            // Style decoding parses CBS too, when the body is published.
            parserMocks.trimMarkdown.mockImplementation((value: string) => {
                noteChatVariableRead('accent')
                return value
            })
            const first = deferred<string>()
            parserMocks.ParseMarkdown.mockImplementationOnce((_message: string, ...args: unknown[]) => {
                ;(args[4] as ParseMarkdownRenderContext).variableReads?.note('hp')
                return first.promise
            })
            const onDisplaySettled = vi.fn()
            mounted = mount(ChatBodyInlayHarness, { target, props: { onDisplaySettled, displayRow: row } })
            await vi.advanceTimersByTimeAsync(0)
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledOnce()
            // A parse in progress counts with what it has read so far.
            expect(displayRowsToRefresh({ keys: ['hp'] })).toEqual([0])
            expect(displayRowsToRefresh({ keys: ['mood'] })).toEqual([])
            first.resolve('<p>first</p>')
            await vi.advanceTimersByTimeAsync(0)
            expect(onDisplaySettled).toHaveBeenCalledOnce()
            expect(displayRowsToRefresh({ keys: ['hp'] })).toEqual([0])
            expect(displayRowsToRefresh({ keys: ['accent'] })).toEqual([0])
            expect(displayRowsToRefresh({ keys: ['mood'] })).toEqual([])

            // A newer parse that has not read anything yet does not hide what is shown.
            parserMocks.ParseMarkdown.mockReturnValueOnce(new Promise(() => {}))
            ;(mounted as { reload(): void }).reload()
            await vi.advanceTimersByTimeAsync(0)
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(2)
            expect(displayRowsToRefresh({ keys: ['hp'] })).toEqual([0])
            expect(displayRowsToRefresh({ keys: ['mood'] })).toEqual([])

            const { translateHTML } = await import('src/ts/translator/translator')
            vi.mocked(translateHTML).mockResolvedValue('<span>translated</span>')
            ;(mounted as { setTranslated(value: boolean): void }).setTranslated(true)
            await vi.advanceTimersByTimeAsync(0)
            expect(target.textContent).toContain('translated')
            // Initial auto-translation state must not overwrite a later manual toggle.
            await vi.runAllTimersAsync()
            expect(target.textContent).toContain('translated')
            expect(displayRowsToRefresh({ keys: ['mood'] })).toEqual([0])
        } finally {
            row.dispose()
        }
    })

    test('keeps translated state reactive when live parsing starts after a yield', async () => {
        chatState.db = {
            autoTranslate: false,
            translatorType: 'mock',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            newImageHandlingBeta: false,
        }
        const { translateHTML } = await import('src/ts/translator/translator')
        vi.mocked(translateHTML).mockResolvedValue('<span>translated</span>')
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() =>
            expect(parserMocks.ParseMarkdown).toHaveBeenCalledOnce(),
        )

        schedulingMocks.yieldToMainThread.mockClear()
        schedulingMocks.state.controlled = true
        ;(mounted as { setTranslated(value: boolean): void }).setTranslated(
            true,
        )
        await domSettled()
        await vi.waitFor(() =>
            expect(schedulingMocks.yieldToMainThread).toHaveBeenCalledOnce(),
        )
        expect(translateHTML).not.toHaveBeenCalled()

        schedulingMocks.releaseNext()
        await vi.waitFor(() => expect(translateHTML).toHaveBeenCalledOnce())
    })

    test('keeps live asset width reactive when parsing starts after a yield', async () => {
        chatState.db = {
            autoTranslate: false,
            translatorType: 'mock',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            hideAllImages: false,
            legacyMediaFindings: false,
            assetMaxDifference: 0.5,
            newImageHandlingBeta: false,
        }
        parserMocks.ParseMarkdown.mockImplementation(
            async () =>
                `<img data-asset-width style="max-width:${String(chatState.db.assetWidth)}rem">`,
        )
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                reactiveAssetWidth: true,
                initialAssetWidth: 2,
                liveCharacter: {
                    type: 'simple',
                    chaId: 'live-character',
                    customscript: [],
                    additionalAssets: [],
                    emotionImages: [],
                } as any,
            },
        })
        await vi.waitFor(() =>
            expect(
                target.querySelector<HTMLElement>('[data-asset-width]')?.style
                    .maxWidth,
            ).toBe('2rem'),
        )

        schedulingMocks.yieldToMainThread.mockClear()
        schedulingMocks.state.controlled = true
        ;(mounted as { setAssetWidth(value: number): void }).setAssetWidth(7)
        await domSettled()
        await vi.waitFor(() =>
            expect(schedulingMocks.yieldToMainThread).toHaveBeenCalledOnce(),
        )

        schedulingMocks.releaseNext()
        await vi.waitFor(() =>
            expect(
                target.querySelector<HTMLElement>('[data-asset-width]')?.style
                    .maxWidth,
            ).toBe('7rem'),
        )
    })

    test('revokes an active mounted URL exactly once on destroy', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first'], { type: 'image/png' }),
            type: 'image',
            name: 'first.png',
        })
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:first'))

        await unmount(mounted)
        mounted = undefined

        expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:first')).toHaveLength(1)
    })

    test('does not reinterpret a viewport-managed inlay as an unresolved bot asset', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        chatState.db = { newImageHandlingBeta: true }
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first']), type: 'image', name: 'first.png',
        })
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() => expect(target.querySelector('img')?.dataset.risuInlayToken).toBeTruthy())
        const image = target.querySelector('img')!

        expect(image.hasAttribute('noimage')).toBe(false)
        expect(image.getAttribute('src')).toBeNull()
        TestIntersectionObserver.instance?.setVisible(image, true)
        await vi.waitFor(() => expect(image.getAttribute('src')).toBe('blob:first'))
        expect(image.hasAttribute('noimage')).toBe(false)
        expect(liveRenderMocks.getFileSrc).not.toHaveBeenCalled()
    })

    test.each(['rerender', 'raw switch', 'destroy'] as const)(
        'does not attach a late object URL after %s cleanup',
        async (cleanupKind) => {
            const firstRead = deferred<{ data: Blob, type: 'image', name: string }>()
            inlayMocks.getInlayAssetBlob.mockImplementation((id: string) => (
                id === 'first'
                    ? firstRead.promise
                    : Promise.resolve({ data: new Blob([id]), type: 'image' as const, name: `${id}.png` })
            ))
            mounted = mount(ChatBodyInlayHarness, { target })
            await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledWith('first'))

            if (cleanupKind === 'rerender') {
                ;(mounted as { setMessage(value: string): void }).setMessage('second')
                await domSettled()
                await vi.waitFor(() => expect(target.querySelector('img')?.dataset.risuInlayId).toBe('second'))
            }
            else if (cleanupKind === 'raw switch') {
                ;(mounted as { setRaw(value: boolean): void }).setRaw(true)
                await domSettled()
            }
            else {
                await unmount(mounted)
                mounted = undefined
            }

            firstRead.resolve({ data: new Blob(['first']), type: 'image', name: 'first.png' })
            await Promise.resolve(); await Promise.resolve(); await domSettled()
            expect(target.querySelector('[src="blob:first"]')).toBeNull()
            expect(createObjectURL.mock.calls.filter(([blob]) => blob.size === 5)).toHaveLength(0)
            expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:first')).toHaveLength(0)
            if (cleanupKind === 'rerender') {
                expect(target.querySelector('[src="blob:second"]')).not.toBeNull()
                expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:second')).toHaveLength(0)
                await unmount(mounted!)
                mounted = undefined
                expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:second')).toHaveLength(1)
            }
        },
    )

    test('clears the registry owned by a stale parse result', async () => {
        const firstParse = deferred<string>()
        let firstRegistry: DeferredInlayMarkerRegistry | undefined
        let exposedMarkup = ''
        parserMocks.ParseMarkdown.mockImplementation(async (message: string, ...args: unknown[]) => {
            const context = args[4] as { deferredInlays?: DeferredInlayMarkerRegistry } | undefined
            const markup = renderDeferredInlaySourceMarkup(message, imageSource, context?.deferredInlays)
            if (message === 'first') {
                firstRegistry = context?.deferredInlays
                exposedMarkup = markup
                return firstParse.promise
            }
            return markup
        })
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first']), type: 'image', name: 'first.png',
        })
        mounted = mount(ChatBodyInlayHarness, { target })
        await vi.waitFor(() => expect(firstRegistry).toBeDefined())

        const clearRegistry = vi.spyOn(firstRegistry!, 'clear')
        ;(mounted as { setMessage(value: string): void }).setMessage('second')
        await domSettled()
        await vi.waitFor(() => expect(target.querySelector('img')?.dataset.risuInlayId).toBe('second'))
        expect(clearRegistry).toHaveBeenCalled()
        firstParse.resolve(exposedMarkup)
        await Promise.resolve(); await Promise.resolve(); await domSettled()

        const stolen = document.createElement('div')
        stolen.innerHTML = exposedMarkup
        document.body.append(stolen)
        mountDeferredInlaySources(stolen, firstRegistry)
        await Promise.resolve(); await Promise.resolve()
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalledWith('first')
    })

    test('reports only the latest parse generation as capture settled', async () => {
        const firstParse = deferred<string>()
        parserMocks.ParseMarkdown.mockImplementation(async (message: string) => {
            if (message === 'first') return firstParse.promise
            return `<span>${message}</span>`
        })
        const onCaptureSettled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onCaptureSettled } })
        await vi.waitFor(() => expect(parserMocks.ParseMarkdown).toHaveBeenCalled())

        ;(mounted as { setMessage(value: string): void }).setMessage('second')
        await domSettled()
        await vi.waitFor(() => expect(onCaptureSettled).toHaveBeenCalledOnce())
        const activeGeneration = onCaptureSettled.mock.calls[0][0]

        firstParse.resolve('<span>first</span>')
        await Promise.resolve(); await Promise.resolve(); await domSettled()

        expect(onCaptureSettled).toHaveBeenCalledOnce()
        expect(activeGeneration).toBeGreaterThan(1)
    })

    test('reports capture settled after its deferred object URLs are mounted', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first']), type: 'image', name: 'first.png',
        })
        const onCaptureSettled = vi.fn(() => {
            expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:first')
        })

        mounted = mount(ChatBodyInlayHarness, { target, props: { onCaptureSettled } })
        await vi.waitFor(() => expect(onCaptureSettled).toHaveBeenCalledOnce())
    })

    test('reports a terminal capture parse failure instead of settling raw markup', async () => {
        const parseError = new Error('terminal capture parse failure')
        parserMocks.ParseMarkdown.mockRejectedValue(parseError)
        const onCaptureSettled = vi.fn()
        const onCaptureError = vi.fn()

        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                captureContext: minimalCaptureContext(),
                onCaptureSettled,
                onCaptureError,
            },
        })

        await vi.waitFor(() => expect(onCaptureError).toHaveBeenCalledWith(
            expect.any(Number),
            parseError,
        ))
        expect(onCaptureSettled).not.toHaveBeenCalled()
        expect(target.textContent).not.toContain('first')
    })

    test('uses frozen capture character, role, settings, and assets without live lookups', async () => {
        parserMocks.ParseMarkdown.mockResolvedValue('<img src="frozen.png">')
        const onCaptureSettled = vi.fn()
        const captureContext = {
            character: {
                type: 'simple' as const,
                chaId: 'frozen-character',
                customscript: [],
                additionalAssets: [['Frozen', 'frozen.png', 'png']] as [string, string, string][],
            },
            characterName: 'Frozen Character',
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen User',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [] as [string, string, string][],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: 'frozen-style',
            parserContext: {
                database: { characters: [] } as any,
                character: {
                    type: 'character' as const,
                    name: 'Frozen Character',
                    chaId: 'frozen-character',
                    chatPage: 0,
                    chats: [{ message: [], note: '', name: '', localLore: [] }],
                    customscript: [],
                } as any,
                userName: 'Frozen User',
                personaPrompt: '',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: false,
                autoTranslateCachedOnly: false,
                translatorType: 'mock',
                translateBeforeHTMLFormatting: false,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: true,
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

        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: { onCaptureSettled, captureContext },
        })
        await vi.waitFor(() => expect(onCaptureSettled).toHaveBeenCalledOnce())

        const parseCall = parserMocks.ParseMarkdown.mock.calls[0]
        expect(parseCall[1]).toMatchObject({ chaId: 'frozen-character' })
        expect(parseCall[4]).toMatchObject({ chatRole: 'char' })
        expect(parseCall[5]).toMatchObject({
            moduleAssets: [],
            scriptContext: expect.objectContaining({ moduleAssets: [] }),
        })
        expect(liveRenderMocks.getModuleAssets).not.toHaveBeenCalled()
        expect(liveRenderMocks.getCurrentCharacter).not.toHaveBeenCalled()
        expect(liveRenderMocks.getFileSrc).toHaveBeenCalledWith('frozen.png')
    })

    test('waits for the translated active generation instead of settling intermediate empty markup', async () => {
        vi.useFakeTimers()
        chatState.db = {
            autoTranslate: true,
            autoTranslateCachedOnly: false,
            translatorType: 'mock',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            newImageHandlingBeta: false,
        }
        parserMocks.ParseMarkdown.mockResolvedValue('<span>parsed</span>')
        const { translateHTML } = await import('src/ts/translator/translator')
        vi.mocked(translateHTML).mockResolvedValue('<span>translated</span>')
        const onCaptureSettled = vi.fn()

        mounted = mount(ChatBodyInlayHarness, { target, props: { onCaptureSettled } })
        await Promise.resolve(); await domSettled()

        expect(onCaptureSettled).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(10)
        await domSettled()
        await vi.runAllTimersAsync()
        await domSettled()
        expect(translateHTML).toHaveBeenCalled()
        expect(onCaptureSettled).toHaveBeenCalledOnce()
    })

    test('passes the immutable capture parser context into auto-translation', async () => {
        vi.useFakeTimers()
        parserMocks.ParseMarkdown.mockResolvedValue('<span>parsed</span>')
        const { translateHTML } = await import('src/ts/translator/translator')
        vi.mocked(translateHTML).mockResolvedValue('<span>translated</span>')
        const character = {
            type: 'character' as const,
            name: 'Frozen Character',
            chaId: 'frozen-character',
            chatPage: 0,
            chats: [{ message: [], note: '', name: '', localLore: [] }],
            customscript: [],
        }
        const captureContext = {
            character: {
                type: 'simple' as const,
                chaId: 'frozen-character',
                customscript: [],
            },
            characterName: 'Frozen Character',
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen User',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: { characters: [character] } as any,
                character: character as any,
                userName: 'Frozen User',
                personaPrompt: 'Frozen Persona',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
                historyOffset: 4,
            },
            settings: {
                autoTranslate: true,
                autoTranslateCachedOnly: false,
                translatorType: 'mock',
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

        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                captureContext,
                idx: 6,
                captureParserIndex: 2,
                name: 'Frozen Character',
            },
        })
        await Promise.resolve(); await domSettled()
        await vi.advanceTimersByTimeAsync(10)
        await domSettled()
        await vi.runAllTimersAsync()
        await domSettled()

        expect(translateHTML).toHaveBeenCalledWith(
            expect.any(String),
            false,
            expect.objectContaining({ chaId: 'frozen-character' }),
            6,
            false,
            expect.objectContaining({
                projectedChatID: 2,
                chara: expect.objectContaining({ name: 'Frozen Character' }),
                scriptContext: expect.objectContaining({
                    parserContext: expect.objectContaining({ historyOffset: 4 }),
                }),
            }),
            expect.any(AbortSignal),
        )
        expect(vi.mocked(translateHTML).mock.calls[0][5]).not.toHaveProperty('liveProjection')
    })

    function boundedLiveProjection() {
        const capture = minimalCaptureContext()
        capture.parserContext.historyOffset = 4
        capture.parserContext.character.chats[0].message = [
            { role: 'char', data: 'previous' },
            { role: 'user', data: 'nearby' },
            { role: 'char', data: 'current' },
        ]
        capture.parserContext.database.characters[0] = capture.parserContext.character
        const parserProjection = {
            kind: 'bounded' as const,
            characterId: 'frozen-character',
            conversationId: 'conversation',
            revision: 1,
            totalMessages: 7,
            chatID: 6,
            projectedChatID: 2,
            historyOffset: 4,
            messages: capture.parserContext.character.chats[0].message,
            context: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                parserContext: capture.parserContext,
            },
        }
        return parserProjection
    }

    test('passes a bounded live parser projection with absolute and projected indices', async () => {
        parserMocks.ParseMarkdown.mockResolvedValue('<span>projected</span>')
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                idx: 6,
                name: 'Projected Character',
                parserProjection: boundedLiveProjection() as any,
            },
        })

        await vi.waitFor(() => expect(parserMocks.ParseMarkdown).toHaveBeenCalled())
        expect(parserMocks.ParseMarkdown.mock.calls[0]).toEqual([
            'first',
            expect.objectContaining({ chaId: 'frozen-character' }),
            'notrim',
            6,
            expect.objectContaining({ chatRole: 'char' }),
            expect.objectContaining({
                projectedChatID: 2,
                scriptContext: expect.objectContaining({
                    parserContext: expect.objectContaining({ historyOffset: 4 }),
                }),
            }),
        ])
    })

    test('translates a bounded live projection as a row of the live chat', async () => {
        chatState.db = { autoTranslate: true, translatorType: 'mock', legacyTranslation: false }
        parserMocks.ParseMarkdown.mockResolvedValue('<span>projected</span>')
        const { translateHTML } = await import('src/ts/translator/translator')
        vi.mocked(translateHTML).mockResolvedValue('<span>translated</span>')
        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                idx: 6,
                name: 'Projected Character',
                parserProjection: boundedLiveProjection() as any,
                initialTranslated: true,
            },
        })

        await vi.waitFor(() => expect(translateHTML).toHaveBeenCalled())
        expect(vi.mocked(translateHTML).mock.calls[0][5]).toMatchObject({ liveProjection: true, projectedChatID: 2 })
    })

    test('does not report capture settled after a pending body is destroyed', async () => {
        const pendingParse = deferred<string>()
        parserMocks.ParseMarkdown.mockReturnValue(pendingParse.promise)
        const onCaptureSettled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onCaptureSettled } })
        await vi.waitFor(() => expect(parserMocks.ParseMarkdown).toHaveBeenCalled())

        await unmount(mounted)
        mounted = undefined
        pendingParse.resolve('<span>late</span>')
        await Promise.resolve(); await Promise.resolve(); await domSettled()

        expect(onCaptureSettled).not.toHaveBeenCalled()
    })

    test('does not let translated markup reuse an observed slot for another asset', async () => {
        chatState.db = {
            autoTranslate: true,
            translatorType: 'mock',
            legacyTranslation: false,
        }
        parserMocks.ParseMarkdown.mockImplementation(async (message: string, ...args: unknown[]) => {
            const context = args[4] as { deferredInlays?: DeferredInlayMarkerRegistry } | undefined
            return renderDeferredInlaySourceMarkup(message, imageSource, context?.deferredInlays)
        })
        const { translateHTML } = await import('src/ts/translator/translator')
        vi.mocked(translateHTML).mockImplementation(async (markup: string) => {
            expect(markup).toContain('data-risu-inlay-slot')
            expect(markup).not.toContain('data-risu-inlay-token')
            return `${markup.replace('data-risu-inlay-id="first"', 'data-risu-inlay-id="other"')}${markup}`
        })
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['first']), type: 'image', name: 'first.png',
        })

        mounted = mount(ChatBodyInlayHarness, { target, props: { initialTranslated: true } })
        await vi.waitFor(() => expect(target.querySelector('[src="blob:first"]')).not.toBeNull())

        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(1)
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledWith('first')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalledWith('other')
        expect(target.querySelectorAll('[src="blob:first"]')).toHaveLength(1)
    })
})

describe('ChatBody settings navigation round trip', () => {
    // Stand-ins for the native alias table and the loopback media endpoint. Only
    // synthetic hashes are used; no image bytes are read.
    const endpoint = `http://127.0.0.1:4100/${'a'.repeat(32)}/`
    const aliases = new Map<string, { hash: string, revision: number }>()
    const parseGates: Array<Promise<void>> = []
    const bodies: Array<ReturnType<typeof mount>> = []
    const nativeUrl = (key: string) => {
        const alias = aliases.get(key)
        return alias ? `${endpoint}${alias.hash}?mime=image%2Fpng&size=1` : null
    }

    class ViewportObserver {
        static live = new Set<ViewportObserver>()
        readonly targets = new Set<Element>()
        constructor(private readonly callback: IntersectionObserverCallback) {
            ViewportObserver.live.add(this)
        }
        observe = (element: Element) => { this.targets.add(element) }
        unobserve = (element: Element) => { this.targets.delete(element) }
        disconnect = () => {
            ViewportObserver.live.delete(this)
            this.targets.clear()
        }
        takeRecords = () => []
        readonly root = null
        readonly rootMargin = '0px'
        readonly thresholds = [0]
        show() {
            this.callback([...this.targets].map((target) => ({
                target,
                isIntersecting: true,
                intersectionRatio: 1,
            }) as IntersectionObserverEntry), this as unknown as IntersectionObserver)
        }
    }

    const showAll = () => { for (const observer of [...ViewportObserver.live]) observer.show() }
    const mountBody = (message: string) => {
        const host = document.createElement('div')
        document.body.append(host)
        const body = mount(ChatBodyInlayHarness, { target: host, props: { initialMessage: message } })
        bodies.push(body)
        return { host, body }
    }
    const closeBody = async (body: ReturnType<typeof mount>) => {
        bodies.splice(bodies.indexOf(body), 1)
        await unmount(body)
    }
    const displayedSources = (host: ParentNode) =>
        [...host.querySelectorAll('img')].map((image) => image.getAttribute('src'))

    beforeEach(() => {
        vi.clearAllMocks()
        vi.stubGlobal('IntersectionObserver', ViewportObserver)
        ViewportObserver.live.clear()
        aliases.clear()
        parseGates.length = 0
        chatState.db = {}
        schedulingMocks.state.controlled = false
        parserMocks.trimMarkdown.mockImplementation((value: string) => value)
    })

    afterEach(async () => {
        for (const body of bodies.splice(0)) await unmount(body)
        document.body.replaceChildren()
        vi.unstubAllGlobals()
    })

    test('restores native display URLs after leaving settings and follows a same-key replacement', async () => {
        const revokeObjectURL = vi.fn()
        const createObjectURL = vi.fn(() => 'blob:unexpected')
        vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL })
        parserMocks.ParseMarkdown.mockImplementation(async (message: string, ...args: unknown[]) => {
            // The URL is resolved before any delay so a late result carries its old identity.
            const url = nativeUrl(message)
            const gate = parseGates.shift()
            if (gate) await gate
            if (!url) return ''
            const context = args[4] as { deferredInlays?: DeferredInlayMarkerRegistry } | undefined
            const inlay = renderDeferredInlaySourceMarkup(message, { ...imageSource, url }, context?.deferredInlays)
            return `${inlay}<img data-synthetic-asset src="${url}">`
        })
        aliases.set('synthetic', { hash: '11'.repeat(32), revision: 1 })
        const firstUrl = nativeUrl('synthetic')!

        // Two display owners of the same key, such as a chat body and a background layer.
        const chat = mountBody('synthetic')
        const other = mountBody('synthetic')
        await vi.waitFor(() => expect(document.querySelectorAll('img[data-risu-inlay-token]')).toHaveLength(2))
        expect(document.querySelectorAll('img[data-synthetic-asset][src]')).toHaveLength(2)
        showAll()
        await vi.waitFor(() => expect(displayedSources(document)).toEqual([firstUrl, firstUrl, firstUrl, firstUrl]))

        // Entering settings unmounts the chat branch; one owner leaving must not strip the other.
        await closeBody(chat.body)
        expect(displayedSources(other.host)).toEqual([firstUrl, firstUrl])
        await closeBody(other.body)
        expect(ViewportObserver.live.size).toBe(0)

        // A view that started resolving just before navigation settles while settings are open.
        const stale = deferred<void>()
        parseGates.push(stale.promise)
        const abandoned = mountBody('synthetic')
        await vi.waitFor(() => expect(parserMocks.ParseMarkdown).toHaveBeenCalledTimes(3))
        await closeBody(abandoned.body)

        // The same logical key now names a new object revision.
        aliases.set('synthetic', { hash: '22'.repeat(32), revision: 2 })
        const replacedUrl = nativeUrl('synthetic')!
        const returned = mountBody('synthetic')
        await vi.waitFor(() => expect(returned.host.querySelectorAll('img[data-risu-inlay-token]')).toHaveLength(1))
        showAll()
        await vi.waitFor(() => expect(displayedSources(returned.host)).toEqual([replacedUrl, replacedUrl]))

        stale.resolve()
        await Promise.resolve(); await Promise.resolve(); await domSettled()
        expect(displayedSources(document)).toEqual([replacedUrl, replacedUrl])
        expect(aliases.get('synthetic')?.revision).toBe(2)
        expect(createObjectURL).not.toHaveBeenCalled()
        expect(revokeObjectURL).not.toHaveBeenCalled()
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
    })

    test('keeps each object URL lease per owner and retries a failed read after leaving settings', async () => {
        let leases = 0
        const createObjectURL = vi.fn(() => `blob:lease-${++leases}`)
        const revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL })
        parserMocks.ParseMarkdown.mockImplementation(async (message: string, ...args: unknown[]) => {
            const context = args[4] as { deferredInlays?: DeferredInlayMarkerRegistry } | undefined
            return renderDeferredInlaySourceMarkup(message, imageSource, context?.deferredInlays)
        })
        inlayMocks.getInlayAssetBlob
            .mockRejectedValueOnce(new Error('transient read failure'))
            .mockImplementation(async (id: string) => ({
                data: new Blob([id], { type: 'image/png' }),
                type: 'image',
                name: `${id}.png`,
            }))

        const failed = mountBody('synthetic')
        await vi.waitFor(() => expect(failed.host.querySelector('img[data-risu-inlay-token]')).not.toBeNull())
        showAll()
        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(1))
        await Promise.resolve(); await domSettled()
        expect(displayedSources(failed.host)).toEqual([null])

        // Settings round trip: the failure was not remembered as a missing asset.
        await closeBody(failed.body)
        const chat = mountBody('synthetic')
        const other = mountBody('synthetic')
        await vi.waitFor(() => expect(document.querySelectorAll('img[data-risu-inlay-token]')).toHaveLength(2))
        showAll()
        await vi.waitFor(() => expect(displayedSources(document).every((source) => source?.startsWith('blob:lease-'))).toBe(true))
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(3)
        const chatLease = displayedSources(chat.host)[0]!
        const otherLease = displayedSources(other.host)[0]!
        expect(chatLease).not.toBe(otherLease)

        await closeBody(chat.body)
        expect(revokeObjectURL.mock.calls).toEqual([[chatLease]])
        expect(displayedSources(other.host)).toEqual([otherLease])
        await closeBody(other.body)
        expect(revokeObjectURL.mock.calls).toEqual([[chatLease], [otherLease]])
    })
})
