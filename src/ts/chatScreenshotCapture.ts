import type { StreamingScreenshotArchive } from './chatScreenshotArchive'
import type { ChatScreenshotJob, FrozenChatScreenshotRenderContext } from './chatScreenshotRange'

export const CHAT_SCREENSHOT_WIDTH = 960
export const CHAT_SCREENSHOT_PAGE_HEIGHT = 8_000
export const CHAT_SCREENSHOT_BATCH_SIZE = 8
export const CHAT_SCREENSHOT_RESOURCE_TIMEOUT_MS = 5_000
export const CHAT_SCREENSHOT_PARSE_TIMEOUT_MS = 10_000

type ScreenshotMessage = ChatScreenshotJob['messages'][number]

export interface ChatScreenshotSurface {
    mountBatch(
        messages: readonly ScreenshotMessage[],
        firstTurn: number,
        renderContext: FrozenChatScreenshotRenderContext,
        signal: AbortSignal,
    ): Promise<HTMLElement>
    unmountBatch(): void | Promise<void>
    dispose(): void | Promise<void>
}

export interface ChatScreenshotEncoder {
    prepare(root: HTMLElement, signal: AbortSignal): Promise<void>
    measureHeight(root: HTMLElement): number
    encodeTile(
        root: HTMLElement,
        tile: Readonly<{ offset: number; height: number; width: number }>,
        signal: AbortSignal,
    ): Promise<Blob>
}

export interface ChatScreenshotOutput {
    publishPng(page: Blob): Promise<boolean>
    createArchive(): Promise<StreamingScreenshotArchive>
}

export interface ChatScreenshotProgress {
    completedTurns: number
    totalTurns: number
}

export type ChatScreenshotResult = Readonly<{
    kind: 'png' | 'zip'
    pages: number
}>

function throwIfAborted(signal: AbortSignal) {
    if (signal.aborted) throw new DOMException('Screenshot capture was cancelled', 'AbortError')
}

export function canExportLongScreenshotArchive(
    isNative: boolean,
    isNativeDesktop: boolean,
    isAndroidSafReady = false,
) {
    return !isNative || isNativeDesktop || isAndroidSafReady
}

export async function captureChatScreenshot(
    job: ChatScreenshotJob,
    dependencies: {
        surface: ChatScreenshotSurface
        encoder: ChatScreenshotEncoder
        output: ChatScreenshotOutput
        signal: AbortSignal
        onProgress?: (progress: ChatScreenshotProgress) => void
    },
): Promise<ChatScreenshotResult> {
    const { surface, encoder, output, signal, onProgress } = dependencies
    const batchCount = Math.ceil(job.messages.length / CHAT_SCREENSHOT_BATCH_SIZE)
    let archive: StreamingScreenshotArchive | null = null
    let pngCommitted = false
    let pageNumber = 0

    try {
        throwIfAborted(signal)
        if (batchCount > 1) archive = await output.createArchive()

        for (let batchIndex = 0; batchIndex < batchCount; batchIndex++) {
            const batchStart = batchIndex * CHAT_SCREENSHOT_BATCH_SIZE
            const batch = job.messages.slice(
                batchStart,
                batchStart + CHAT_SCREENSHOT_BATCH_SIZE,
            )
            let batchStarted = false
            try {
                throwIfAborted(signal)
                batchStarted = true
                const root = await surface.mountBatch(
                    batch,
                    job.start + batchStart,
                    job.renderContext,
                    signal,
                )
                throwIfAborted(signal)
                await encoder.prepare(root, signal)
                throwIfAborted(signal)

                const contentHeight = Math.max(1, Math.ceil(encoder.measureHeight(root)))
                const tileCount = Math.ceil(contentHeight / CHAT_SCREENSHOT_PAGE_HEIGHT)
                if (!archive && tileCount > 1) archive = await output.createArchive()

                for (let tileIndex = 0; tileIndex < tileCount; tileIndex++) {
                    throwIfAborted(signal)
                    const offset = tileIndex * CHAT_SCREENSHOT_PAGE_HEIGHT
                    const page = await encoder.encodeTile(
                        root,
                        {
                            offset,
                            height: Math.min(
                                CHAT_SCREENSHOT_PAGE_HEIGHT,
                                contentHeight - offset,
                            ),
                            width: CHAT_SCREENSHOT_WIDTH,
                        },
                        signal,
                    )
                    throwIfAborted(signal)
                    pageNumber++
                    if (archive) {
                        await archive.addPage(pageNumber, page)
                    } else {
                        const published = await output.publishPng(page)
                        if (!published) {
                            throw new DOMException('Screenshot export was cancelled', 'AbortError')
                        }
                        pngCommitted = true
                    }
                    if (!pngCommitted) throwIfAborted(signal)
                }
                onProgress?.({
                    completedTurns: Math.min(batchStart + batch.length, job.messages.length),
                    totalTurns: job.messages.length,
                })
            } finally {
                if (batchStarted) await surface.unmountBatch()
            }
        }

        if (!pngCommitted) throwIfAborted(signal)
        const committedAfterCancellation = archive ? await archive.close(signal) : pngCommitted
        if (!committedAfterCancellation) throwIfAborted(signal)
        return { kind: archive ? 'zip' : 'png', pages: pageNumber }
    } catch (error) {
        if (archive) await archive.abort()
        throw error
    } finally {
        await surface.dispose()
    }
}

function capturePlaceholder(kind: string) {
    const placeholder = document.createElement('div')
    placeholder.dataset.screenshotPlaceholder = kind
    placeholder.textContent = `[${kind}]`
    placeholder.style.cssText =
        'display:flex;align-items:center;justify-content:center;min-height:48px;border:1px solid currentColor;opacity:.7;'
    return placeholder
}

export function replaceCaptureMedia(root: HTMLElement) {
    for (const element of root.querySelectorAll('audio, video, iframe, canvas')) {
        const kind = element.tagName.toLowerCase()
        if (element instanceof HTMLMediaElement) element.pause()
        element.replaceWith(capturePlaceholder(kind))
    }
}

async function withCaptureTimeout<T>(promise: Promise<T>, signal: AbortSignal): Promise<void> {
    await new Promise<void>((resolve, reject) => {
        let settled = false
        const finish = (error?: unknown) => {
            if (settled) return
            settled = true
            clearTimeout(timeout)
            signal.removeEventListener('abort', onAbort)
            if (error) reject(error)
            else resolve()
        }
        const onAbort = () => finish(new DOMException('Screenshot capture was cancelled', 'AbortError'))
        const timeout = setTimeout(() => finish(), CHAT_SCREENSHOT_RESOURCE_TIMEOUT_MS)
        signal.addEventListener('abort', onAbort, { once: true })
        if (signal.aborted) return onAbort()
        promise.then(() => finish(), finish)
    })
}

async function waitForCaptureResources(root: HTMLElement, signal: AbortSignal) {
    const fontReady = document.fonts?.ready ?? Promise.resolve()
    const imageReady = Promise.all(
        Array.from(root.querySelectorAll('img')).map((image) => {
            if (image.complete) return Promise.resolve()
            return new Promise<void>((resolve) => {
                image.addEventListener('load', () => resolve(), { once: true })
                image.addEventListener('error', () => resolve(), { once: true })
            })
        }),
    )
    await withCaptureTimeout(Promise.all([fontReady, imageReady]), signal)
}

export function createDomScreenshotEncoder(): ChatScreenshotEncoder {
    return {
        async prepare(root, signal) {
            replaceCaptureMedia(root)
            await waitForCaptureResources(root, signal)
        },

        measureHeight(root) {
            const content =
                root.querySelector<HTMLElement>('[data-screenshot-content]') ?? root
            return Math.max(content.scrollHeight, content.getBoundingClientRect().height, 1)
        },

        async encodeTile(root, tile, signal) {
            throwIfAborted(signal)
            const content =
                root.querySelector<HTMLElement>('[data-screenshot-content]') ?? root
            const previousHeight = root.style.height
            const previousTransform = content.style.transform
            const previousTransformOrigin = content.style.transformOrigin
            root.style.height = `${tile.height}px`
            content.style.transform = `translateY(-${tile.offset}px)`
            content.style.transformOrigin = 'top left'
            try {
                const { toBlob } = await import('html-to-image')
                throwIfAborted(signal)
                const page = await toBlob(root, {
                    width: tile.width,
                    height: tile.height,
                    canvasWidth: tile.width,
                    canvasHeight: tile.height,
                    pixelRatio: 1,
                    skipAutoScale: true,
                    backgroundColor: getComputedStyle(root).backgroundColor || '#fff',
                    style: {
                        position: 'static',
                        left: 'auto',
                        top: 'auto',
                        pointerEvents: 'none',
                    },
                })
                throwIfAborted(signal)
                if (!page) throw new Error('Screenshot encoder returned no PNG data')
                return page
            } finally {
                root.style.height = previousHeight
                content.style.transform = previousTransform
                content.style.transformOrigin = previousTransformOrigin
            }
        },
    }
}
