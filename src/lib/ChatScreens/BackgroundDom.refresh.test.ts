// @vitest-environment happy-dom

import { afterEach, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const state = vi.hoisted(() => ({
    character: {
        chaId: 'synthetic',
        backgroundHTML: '<style>.panel { color: red }</style>',
    },
    selection: 'conversation-one',
    notify: null as null | ((identity: string) => void),
    acquire: vi.fn(),
    parse: vi.fn(),
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { characters: [state.character] } },
        selIdState: { selId: 0 },
        moduleBackgroundEmbedding: writable(''),
        ReloadGUIPointer: writable(0),
    }
})
vi.mock('src/ts/storage/database.svelte', () => ({
    getCurrentCharacter: () => state.character,
    getCurrentChat: () => ({ id: state.selection }),
}))
vi.mock('src/ts/liveDisplayParserLease', () => ({
    captureLiveDisplayParserInputs: (source: unknown) => ({ source }),
    captureLiveDisplayParserSelection: () => state.selection,
    subscribeLiveDisplayParserSelection: (
        notify: (identity: string) => void,
    ) => {
        state.notify = notify
        return () => {
            state.notify = null
        }
    },
    acquireLiveDisplayParserLease: state.acquire,
}))
vi.mock('src/ts/parser/parser.svelte', () => ({
    risuChatParser: (value: string) => value,
    ParseMarkdown: state.parse,
}))
vi.mock('src/ts/process/files/inlayRenderSource', () => ({
    DeferredInlayMarkerRegistry: class {
        clear() {}
    },
    mountDeferredInlaySources: () => () => {},
}))

import BackgroundDom from './BackgroundDom.svelte'
import { ReloadGUIPointer } from 'src/ts/stores.svelte'

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (reason: unknown) => void
    const promise = new Promise<T>((done, fail) => {
        resolve = done
        reject = fail
    })
    return { promise, resolve, reject }
}

let component: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    document.body.replaceChildren()
    vi.resetAllMocks()
    ReloadGUIPointer.set(0)
    state.selection = 'conversation-one'
})

test('keeps shared CSS through delayed admission, repeated reloads and stale parse completion', async () => {
    const first = '<style>.panel { color: red }</style><div>first</div>'
    const latest = '<style>.panel { color: blue }</style><div>latest</div>'
    state.acquire.mockResolvedValue({ release: vi.fn() })
    state.parse.mockResolvedValue(first)
    const host = document.createElement('div')
    document.body.append(host)
    component = mount(BackgroundDom, { target: host })
    await vi.waitFor(() => expect(host.querySelector('style')).not.toBeNull())
    const oldStyle = host.querySelector('style')
    const admission = deferred<{ release(): void }>()
    const stale = deferred<string>()
    const newest = deferred<string>()
    state.acquire.mockReturnValueOnce(admission.promise)
    state.parse
        .mockReturnValueOnce(stale.promise)
        .mockReturnValueOnce(newest.promise)
    ReloadGUIPointer.update((value) => value + 1)
    await tick()
    expect(host.querySelector('style')).toBe(oldStyle)
    admission.resolve({ release: vi.fn() })
    await vi.waitFor(() => expect(state.parse).toHaveBeenCalledTimes(2))
    expect(host.querySelector('style')).toBe(oldStyle)
    ReloadGUIPointer.update((value) => value + 1)
    await vi.waitFor(() => expect(state.parse).toHaveBeenCalledTimes(3))
    expect(host.querySelector('style')).toBe(oldStyle)
    newest.resolve(latest)
    await vi.waitFor(() => expect(host.textContent).toContain('latest'))
    stale.resolve('<div>stale, missing stylesheet</div>')
    await tick()
    expect(host.querySelector('style')?.textContent).toContain('blue')
    expect(host.textContent).not.toContain('stale')

    for (let i = 0; i < 16; i++) {
        const next = deferred<string>()
        state.parse.mockReturnValueOnce(next.promise)
        const previousStyle = host.querySelector('style')
        ReloadGUIPointer.update((value) => value + 1)
        await vi.waitFor(() => expect(state.parse).toHaveBeenCalledTimes(4 + i))
        expect(host.querySelector('style')).toBe(previousStyle)
        next.resolve(
            `<style>.panel { color: blue }</style><div>revision-${i}</div>`,
        )
        await vi.waitFor(() =>
            expect(host.textContent).toContain(`revision-${i}`),
        )
        expect(host.querySelector('style')).not.toBeNull()
    }

    // Styles from another conversation must not survive navigation.
    state.acquire.mockReturnValueOnce(new Promise(() => {}))
    state.selection = 'conversation-two'
    state.notify?.(state.selection)
    await tick()
    expect(host.querySelector('style')).toBeNull()
})

test('retains the last stylesheet on a failed refresh and recovers on the next reload', async () => {
    state.acquire.mockResolvedValue({ release: vi.fn() })
    state.parse.mockResolvedValue(
        '<style>.panel { color: red }</style><div>first</div>',
    )
    const host = document.createElement('div')
    document.body.append(host)
    component = mount(BackgroundDom, { target: host })
    await vi.waitFor(() => expect(host.querySelector('style')).not.toBeNull())
    const style = host.querySelector('style')
    const error = vi.spyOn(console, 'error').mockImplementation(() => {})
    try {
        state.parse.mockRejectedValueOnce(new Error('Synthetic parse failure'))
        ReloadGUIPointer.update((value) => value + 1)
        await vi.waitFor(() =>
            expect(host.querySelector('[role="alert"]')).not.toBeNull(),
        )
        expect(host.querySelector('style')).toBe(style)
        state.parse.mockResolvedValue(
            '<style>.panel { color: blue }</style><div>recovered</div>',
        )
        ReloadGUIPointer.update((value) => value + 1)
        await vi.waitFor(() => expect(host.textContent).toContain('recovered'))
        expect(host.querySelector('style')?.textContent).toContain('blue')
        expect(host.querySelector('[role="alert"]')).toBeNull()
    } finally {
        error.mockRestore()
    }
})
