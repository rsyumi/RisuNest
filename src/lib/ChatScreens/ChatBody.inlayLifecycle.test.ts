// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

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
vi.mock('src/ts/parser/parser.svelte', () => parserMocks)
vi.mock('src/ts/stores.svelte', () => ({ DBState: chatState }))
vi.mock('src/ts/util', () => ({ sleep: vi.fn(() => Promise.resolve()) }))
vi.mock('src/ts/translator/translator', () => ({
    getLLMCache: vi.fn(),
    translateHTML: vi.fn(),
}))
vi.mock('src/ts/process/modules', () => ({ getModuleAssets: liveRenderMocks.getModuleAssets }))
vi.mock('src/ts/storage/database.svelte', () => ({ getCurrentCharacter: liveRenderMocks.getCurrentCharacter }))
vi.mock('src/ts/globalApi.svelte', () => ({ getFileSrc: liveRenderMocks.getFileSrc }))
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
        await tick()
        const details = target.querySelector('details')!
        expect(details.open).toBe(false)
        expect(target.textContent!.length).toBeLessThan(100)
        details.open = true
        details.dispatchEvent(new Event('toggle'))
        await tick()
        expect(details.textContent).toContain(body)
        ;(mounted as { setMessage(value: string): void }).setMessage(
            `<Thoughts>${body}LATEST</Thoughts>Answer`,
        )
        await tick()
        expect(target.querySelector('details')).toBe(details)
        expect(details.open).toBe(true)
        expect(details.textContent).toContain('LATEST')
        expect(parserMocks.ParseMarkdown).not.toHaveBeenCalled()
    })

    test('preserves a live collapsed block that the user expanded through parsed replacements', async () => {
        parserMocks.ParseMarkdown.mockImplementation(
            async (value, _char, _mode, _idx, _conditions, context) => {
                expect(context?.streamingThoughtMode).toBe('collapsed')
                return `<details data-risu-streaming-thought><summary>Thought</summary>${value}</details>`
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
        await tick()
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
        await tick()
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
        await tick()
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
        await tick()
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
        await tick()
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
        await tick()
        const preview = target.querySelector('[data-streaming-thought-preview]')
        expect(target.querySelector('img')).toBeNull()
        expect(preview?.textContent).toContain('<img src=x')
        ;(mounted as { setMessage(value: string): void }).setMessage(
            '<Thoughts>Replaced reasoning</Thoughts>Answer',
        )
        await tick()
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
        await tick()
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:second'))
        expect(revokeObjectURL.mock.calls.filter(([url]) => url === 'blob:first')).toHaveLength(1)

        ;(mounted as { setRaw(value: boolean): void }).setRaw(true)
        await tick()
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
        await tick()
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
        await tick()
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
        await tick()
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
        await tick()
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
        await tick()
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
        await tick()
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
                await tick()
                await vi.waitFor(() => expect(target.querySelector('img')?.dataset.risuInlayId).toBe('second'))
            }
            else if (cleanupKind === 'raw switch') {
                ;(mounted as { setRaw(value: boolean): void }).setRaw(true)
                await tick()
            }
            else {
                await unmount(mounted)
                mounted = undefined
            }

            firstRead.resolve({ data: new Blob(['first']), type: 'image', name: 'first.png' })
            await Promise.resolve(); await Promise.resolve(); await tick()
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
        await tick()
        await vi.waitFor(() => expect(target.querySelector('img')?.dataset.risuInlayId).toBe('second'))
        expect(clearRegistry).toHaveBeenCalled()
        firstParse.resolve(exposedMarkup)
        await Promise.resolve(); await Promise.resolve(); await tick()

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
        await tick()
        await vi.waitFor(() => expect(onCaptureSettled).toHaveBeenCalledOnce())
        const activeGeneration = onCaptureSettled.mock.calls[0][0]

        firstParse.resolve('<span>first</span>')
        await Promise.resolve(); await Promise.resolve(); await tick()

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
        await Promise.resolve(); await tick()

        expect(onCaptureSettled).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(10)
        await tick()
        await vi.runAllTimersAsync()
        await tick()
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
        await Promise.resolve(); await tick()
        await vi.advanceTimersByTimeAsync(10)
        await tick()
        await vi.runAllTimersAsync()
        await tick()

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
    })

    test('passes a bounded live parser projection with absolute and projected indices', async () => {
        parserMocks.ParseMarkdown.mockResolvedValue('<span>projected</span>')
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

        mounted = mount(ChatBodyInlayHarness, {
            target,
            props: {
                idx: 6,
                name: 'Projected Character',
                parserProjection: parserProjection as any,
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

    test('does not report capture settled after a pending body is destroyed', async () => {
        const pendingParse = deferred<string>()
        parserMocks.ParseMarkdown.mockReturnValue(pendingParse.promise)
        const onCaptureSettled = vi.fn()
        mounted = mount(ChatBodyInlayHarness, { target, props: { onCaptureSettled } })
        await vi.waitFor(() => expect(parserMocks.ParseMarkdown).toHaveBeenCalled())

        await unmount(mounted)
        mounted = undefined
        pendingParse.resolve('<span>late</span>')
        await Promise.resolve(); await Promise.resolve(); await tick()

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
