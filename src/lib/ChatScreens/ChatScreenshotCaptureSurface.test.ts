// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import type { Message } from 'src/ts/storage/database.svelte'

const surfaceMocks = vi.hoisted(() => ({
    getFileSrc: vi.fn(async (source: string) => `asset://${source}`),
}))

vi.mock('./Chat.svelte', async () => ({
    default: (await import('./ChatScreenshotProbe.test.svelte')).default,
}))
vi.mock('src/ts/globalApi.svelte', () => ({ getFileSrc: surfaceMocks.getFileSrc }))

import ChatScreenshotCaptureSurface from './ChatScreenshotCaptureSurface.svelte'

type SurfaceInstance = {
    mountBatch(messages: readonly Message[], firstTurn: number, context: any, signal: AbortSignal): Promise<HTMLElement>
    unmountBatch(): Promise<void>
    dispose(): Promise<void>
}

describe('ChatScreenshotCaptureSurface', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        surfaceMocks.getFileSrc.mockReset()
        surfaceMocks.getFileSrc.mockImplementation(async (source: string) => `asset://${source}`)
        target = document.createElement('div')
        document.body.innerHTML = '<main class="default-chat-screen"><div data-live="preserved">live</div></main>'
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
    })

    test('mounts only the current chronological batch outside the live chat subtree', async () => {
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance
        const live = document.querySelector('.default-chat-screen')!.innerHTML
        const controller = new AbortController()

        const root = await surface.mountBatch(
            [
                { role: 'user', data: 'first' },
                { role: 'char', data: 'second' },
            ],
            1,
            renderContext(),
            controller.signal,
        )

        expect(root.closest('.default-chat-screen')).toBeNull()
        expect([...root.querySelectorAll('[data-capture-probe]')].map((element) => element.textContent)).toEqual([
            'first',
            'second',
        ])
        expect(document.querySelector('.default-chat-screen')!.innerHTML).toBe(live)

        await surface.mountBatch([{ role: 'user', data: 'third' }], 3, renderContext(), controller.signal)
        expect(root.querySelectorAll('[data-capture-probe]')).toHaveLength(1)
        expect(root.textContent).toContain('third')
        expect(root.textContent).not.toContain('first')
    })

    test('rejects pending readiness on abort and releases the batch', async () => {
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance
        const controller = new AbortController()
        const pending = surface.mountBatch(
            [{ role: 'char', data: 'pending' }],
            1,
            renderContext(),
            controller.signal,
        )

        controller.abort()
        await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        await surface.unmountBatch()

        expect(target.querySelectorAll('[data-capture-probe]')).toHaveLength(0)
    })

    test('starts abort ownership before a portrait resolver that never settles', async () => {
        surfaceMocks.getFileSrc.mockReturnValue(new Promise(() => {}))
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance
        const controller = new AbortController()
        const pending = surface.mountBatch(
            [{ role: 'char', data: 'first' }],
            1,
            { ...renderContext(), characterImageSource: 'pending.png' },
            controller.signal,
        )
        const rejection = expect(pending).rejects.toMatchObject({ name: 'AbortError' })

        controller.abort()

        await rejection
    })

    test('rejects never-resolving portrait setup when the parent destroys the surface', async () => {
        surfaceMocks.getFileSrc.mockReturnValue(new Promise(() => {}))
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance
        const pending = surface.mountBatch(
            [{ role: 'char', data: 'first' }],
            1,
            { ...renderContext(), characterImageSource: 'pending.png' },
            new AbortController().signal,
        )
        const rejection = expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        await vi.waitFor(() => expect(surfaceMocks.getFileSrc).toHaveBeenCalledWith('pending.png'))

        await unmount(mounted)
        mounted = undefined

        await rejection
    })

    test('rejects capture readiness on parse failure', async () => {
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance

        await expect(surface.mountBatch(
            [{ role: 'char', data: 'error' }],
            1,
            renderContext(),
            new AbortController().signal,
        )).rejects.toThrow('parse failed')
    })

    test('rejects capture readiness on the bounded timeout', async () => {
        vi.useFakeTimers()
        surfaceMocks.getFileSrc.mockReturnValue(new Promise(() => {}))
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance
        const pending = surface.mountBatch(
            [{ role: 'char', data: 'pending' }],
            1,
            { ...renderContext(), characterImageSource: 'pending.png' },
            new AbortController().signal,
        )
        const rejection = expect(pending).rejects.toThrow('timed out')
        await vi.advanceTimersByTimeAsync(10_000)
        await rejection
        vi.useRealTimers()
    })

    test('uses the frozen context passed with the job instead of live chat state', async () => {
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance
        const context = renderContext()
        const mounting = surface.mountBatch(
            [{ role: 'char', data: 'frozen', time: 123 }],
            1,
            context,
            new AbortController().signal,
        )
        const root = await mounting
        expect(root.querySelector('[data-character-name]')?.textContent).toBe('Character')
        expect(root.querySelector('[data-message-time]')?.textContent).toBe('123')
    })

    test('passes absolute and projected indexes from the frozen history offset', async () => {
        mounted = mount(ChatScreenshotCaptureSurface, { target })
        const surface = mounted as SurfaceInstance
        const root = await surface.mountBatch(
            [{ role: 'char', data: 'indexed' }],
            7,
            { ...renderContext(), historyStartIndex: 3 },
            new AbortController().signal,
        )

        expect(root.querySelector('[data-capture-probe]')?.getAttribute('data-index')).toBe('6')
        expect(root.querySelector('[data-capture-probe]')?.getAttribute('data-parser-index')).toBe('3')
    })
})

function renderContext() {
    const character = {
        type: 'character',
        name: 'Character',
        chaId: 'character',
        chatPage: 0,
        chats: [{ message: [], note: '', name: '', localLore: [] }],
        customscript: [],
    }
    return {
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
            database: { characters: [character] },
            character,
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
    }
}
