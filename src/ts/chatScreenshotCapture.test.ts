import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Message } from './storage/database.svelte'
import {
    CHAT_SCREENSHOT_BATCH_SIZE,
    CHAT_SCREENSHOT_PAGE_HEIGHT,
    CHAT_SCREENSHOT_WIDTH,
    captureChatScreenshot,
    createDomScreenshotEncoder,
    canExportLongScreenshotArchive,
    replaceCaptureMedia,
} from './chatScreenshotCapture'
import type { ChatScreenshotJob } from './chatScreenshotRange'

const htmlToImageMocks = vi.hoisted(() => ({
    toBlob: vi.fn(async () => new Blob(['png'], { type: 'image/png' })),
}))

vi.mock('html-to-image', () => htmlToImageMocks)

afterEach(() => vi.useRealTimers())

function job(count: number): ChatScreenshotJob {
    const character = {
        type: 'character' as const,
        name: 'Character',
        chaId: 'character',
        chatPage: 0,
        chats: [{ message: [], note: '', name: '', localLore: [] }],
        customscript: [],
    }
    return {
        characterId: 'character',
        chatId: 'chat',
        start: 11,
        end: 10 + count,
        totalTurns: 100,
        messages: Array.from({ length: count }, (_, index) => ({
            role: index % 2 ? 'char' : 'user',
            data: `message-${index + 1}`,
        })) as readonly Message[],
        renderContext: {
            character: null,
            characterName: 'Character',
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'User',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: { characters: [character] } as any,
                character: character as any,
                userName: 'User',
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
                translatorType: 'google',
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
        },
    }
}

function harness(options: { height?: number; encodeError?: Error } = {}) {
    let mounted = false
    let maxMounted = 0
    const batches: string[][] = []
    const archive = {
        addPage: vi.fn(async (_pageNumber: number, _page: Blob) => {}),
        close: vi.fn(async (_signal?: AbortSignal): Promise<void | boolean> => {}),
        abort: vi.fn(async () => {}),
    }
    const surface = {
        mountBatch: vi.fn(async (messages: readonly Message[]) => {
            mounted = true
            maxMounted = Math.max(maxMounted, messages.length)
            batches.push(messages.map((message) => message.data))
            return document.createElement('div')
        }),
        unmountBatch: vi.fn(async () => {
            mounted = false
        }),
        dispose: vi.fn(async () => {
            mounted = false
        }),
    }
    const encoder = {
        prepare: vi.fn(async () => {}),
        measureHeight: vi.fn(() => options.height ?? 100),
        encodeTile: vi.fn(async (_root, tile: { offset: number }) => {
            if (options.encodeError) throw options.encodeError
            return new Blob([`tile-${tile.offset}`], { type: 'image/png' })
        }),
    }
    const output = {
        publishPng: vi.fn(async () => true),
        createArchive: vi.fn(async () => archive),
    }
    return {
        surface,
        encoder,
        output,
        archive,
        batches,
        get mounted() {
            return mounted
        },
        get maxMounted() {
            return maxMounted
        },
    }
}

describe('bounded chat screenshot capture', () => {
    it('allows long archives on Web, Tauri desktop, and Android with SAF', () => {
        expect(canExportLongScreenshotArchive(false, false, false)).toBe(true)
        expect(canExportLongScreenshotArchive(true, true, false)).toBe(true)
        expect(canExportLongScreenshotArchive(true, false, true)).toBe(true)
        expect(canExportLongScreenshotArchive(true, false, false)).toBe(false)
    })
    it('exports a short bounded batch as one PNG in chronological order', async () => {
        const deps = harness()
        const progress = vi.fn()

        const result = await captureChatScreenshot(job(3), {
            ...deps,
            signal: new AbortController().signal,
            onProgress: progress,
        })

        expect(result).toEqual({ kind: 'png', pages: 1 })
        expect(deps.batches).toEqual([['message-1', 'message-2', 'message-3']])
        expect(deps.surface.mountBatch).toHaveBeenCalledWith(
            job(3).messages,
            11,
            job(3).renderContext,
            expect.any(AbortSignal),
        )
        expect(deps.maxMounted).toBeLessThanOrEqual(CHAT_SCREENSHOT_BATCH_SIZE)
        expect(deps.output.publishPng).toHaveBeenCalledOnce()
        expect(deps.output.createArchive).not.toHaveBeenCalled()
        expect(progress).toHaveBeenLastCalledWith({ completedTurns: 3, totalTurns: 3 })
        expect(deps.mounted).toBe(false)
        expect(deps.surface.dispose).toHaveBeenCalledOnce()
    })

    it('streams batches and over-height tiles into ordered ZIP pages', async () => {
        const deps = harness({ height: CHAT_SCREENSHOT_PAGE_HEIGHT + 10 })

        const result = await captureChatScreenshot(job(CHAT_SCREENSHOT_BATCH_SIZE + 1), {
            ...deps,
            signal: new AbortController().signal,
        })

        expect(result).toEqual({ kind: 'zip', pages: 4 })
        expect(deps.batches).toHaveLength(2)
        expect(deps.encoder.encodeTile.mock.calls.map((call) => call[1])).toEqual([
            { offset: 0, height: CHAT_SCREENSHOT_PAGE_HEIGHT, width: CHAT_SCREENSHOT_WIDTH },
            { offset: CHAT_SCREENSHOT_PAGE_HEIGHT, height: 10, width: CHAT_SCREENSHOT_WIDTH },
            { offset: 0, height: CHAT_SCREENSHOT_PAGE_HEIGHT, width: CHAT_SCREENSHOT_WIDTH },
            { offset: CHAT_SCREENSHOT_PAGE_HEIGHT, height: 10, width: CHAT_SCREENSHOT_WIDTH },
        ])
        expect(deps.archive.addPage.mock.calls.map((call) => call[0])).toEqual([1, 2, 3, 4])
        expect(deps.archive.close).toHaveBeenCalledOnce()
        expect(deps.archive.abort).not.toHaveBeenCalled()
    })

    it('reports a native ZIP as saved when cancellation loses to atomic publication', async () => {
        const controller = new AbortController()
        const deps = harness()
        deps.archive.close.mockImplementation(async () => {
            controller.abort()
            return true
        })

        const result = await captureChatScreenshot(job(CHAT_SCREENSHOT_BATCH_SIZE + 1), {
            ...deps,
            signal: controller.signal,
        })

        expect(result).toEqual({ kind: 'zip', pages: 2 })
        expect(deps.archive.abort).not.toHaveBeenCalled()
        expect(controller.signal.aborted).toBe(true)
    })

    it('aborts output and releases the mounted surface after cancellation', async () => {
        const deps = harness()
        const controller = new AbortController()
        deps.encoder.prepare.mockImplementationOnce(async () => controller.abort())

        await expect(
            captureChatScreenshot(job(CHAT_SCREENSHOT_BATCH_SIZE + 1), {
                ...deps,
                signal: controller.signal,
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })

        expect(deps.encoder.encodeTile).not.toHaveBeenCalled()
        expect(deps.archive.abort).toHaveBeenCalledOnce()
        expect(deps.archive.close).not.toHaveBeenCalled()
        expect(deps.surface.unmountBatch).toHaveBeenCalledOnce()
        expect(deps.surface.dispose).toHaveBeenCalledOnce()
        expect(deps.mounted).toBe(false)
    })

    it('treats native picker cancellation as capture cancellation', async () => {
        const deps = harness()
        deps.output.publishPng.mockResolvedValueOnce(false)

        await expect(
            captureChatScreenshot(job(1), {
                ...deps,
                signal: new AbortController().signal,
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })

        expect(deps.surface.dispose).toHaveBeenCalledOnce()
    })

    it('checks cancellation after publishing a page and through archive close', async () => {
        const pngDeps = harness()
        const pngController = new AbortController()
        pngDeps.output.publishPng.mockImplementationOnce(async () => {
            pngController.abort()
            return true
        })

        await expect(
            captureChatScreenshot(job(1), {
                ...pngDeps,
                signal: pngController.signal,
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })

        const zipDeps = harness()
        const zipController = new AbortController()
        zipDeps.archive.close.mockImplementationOnce(async (signal?: AbortSignal) => {
            expect(signal).toBe(zipController.signal)
            zipController.abort()
        })

        await expect(
            captureChatScreenshot(job(CHAT_SCREENSHOT_BATCH_SIZE + 1), {
                ...zipDeps,
                signal: zipController.signal,
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(zipDeps.archive.abort).toHaveBeenCalledOnce()
    })

    it('aborts partial output and cleans the surface after encoder failure', async () => {
        const error = new Error('encoding failed')
        const deps = harness({ encodeError: error })

        await expect(
            captureChatScreenshot(job(CHAT_SCREENSHOT_BATCH_SIZE + 1), {
                ...deps,
                signal: new AbortController().signal,
            }),
        ).rejects.toBe(error)

        expect(deps.archive.abort).toHaveBeenCalledOnce()
        expect(deps.surface.unmountBatch).toHaveBeenCalledOnce()
        expect(deps.surface.dispose).toHaveBeenCalledOnce()
    })
})

describe('capture-only media replacement', () => {
    it('pauses media and replaces media, iframes, and canvases with placeholders', () => {
        const root = document.createElement('div')
        root.innerHTML = '<audio></audio><video></video><iframe></iframe><canvas></canvas>'
        const pause = vi.fn()
        for (const media of root.querySelectorAll('audio, video')) {
            Object.defineProperty(media, 'pause', { value: pause })
        }

        replaceCaptureMedia(root)

        expect(pause).toHaveBeenCalledTimes(2)
        expect(root.querySelectorAll('audio, video, iframe, canvas')).toHaveLength(0)
        expect(
            Array.from(root.querySelectorAll('[data-screenshot-placeholder]')).map(
                (element) => element.textContent,
            ),
        ).toEqual(['[audio]', '[video]', '[iframe]', '[canvas]'])
    })

    it('captures with static clone positioning and restores tile styles', async () => {
        const root = document.createElement('div')
        root.style.position = 'fixed'
        root.style.left = '-100000px'
        const content = document.createElement('div')
        content.dataset.screenshotContent = ''
        content.style.transform = 'scale(1)'
        root.append(content)
        const encoder = createDomScreenshotEncoder()

        await encoder.encodeTile(
            root,
            { offset: 100, height: 200, width: CHAT_SCREENSHOT_WIDTH },
            new AbortController().signal,
        )

        expect(htmlToImageMocks.toBlob).toHaveBeenCalledWith(
            root,
            expect.objectContaining({
                style: expect.objectContaining({ position: 'static', left: 'auto', top: 'auto' }),
            }),
        )
        expect(root.style.position).toBe('fixed')
        expect(root.style.left).toBe('-100000px')
        expect(content.style.transform).toBe('scale(1)')
    })

    it('bounds the wait for an image that never settles', async () => {
        vi.useFakeTimers()
        const root = document.createElement('div')
        const image = document.createElement('img')
        Object.defineProperty(image, 'complete', { value: false })
        root.append(image)
        const encoder = createDomScreenshotEncoder()

        const preparing = encoder.prepare(root, new AbortController().signal)
        await vi.advanceTimersByTimeAsync(5_000)

        await expect(preparing).resolves.toBeUndefined()
    })
})
