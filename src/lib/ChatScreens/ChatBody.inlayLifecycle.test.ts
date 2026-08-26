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
        vi.stubGlobal('IntersectionObserver', undefined)
        chatState.db = {}
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
        document.body.replaceChildren()
        vi.unstubAllGlobals()
        vi.useRealTimers()
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
        )
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
