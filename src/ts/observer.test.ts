import { afterEach, describe, expect, it, vi } from 'vitest'

const download = vi.hoisted(() => vi.fn(async () => {}))
vi.mock('./globalApi.svelte', () => ({
    downloadFile: download,
    globalFetch: vi.fn(),
}))
import { startObserveDom } from './observer.svelte'

let stop: (() => void) | undefined
afterEach(() => {
    stop?.()
    stop = undefined
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
    vi.useRealTimers()
})

const settle = () => new Promise<void>((resolve) => setTimeout(resolve, 0))
function openMenu(node: Element) {
    node.dispatchEvent(
        new MouseEvent('contextmenu', { bubbles: true, cancelable: true }),
    )
}

describe('rendered DOM observation', () => {
    it('keeps new audio owned after an old session play promise rejects', async () => {
        let rejectOld!: (error: Error) => void
        const audio: Array<
            EventTarget & {
                play: ReturnType<typeof vi.fn>
                pause: ReturnType<typeof vi.fn>
                remove: ReturnType<typeof vi.fn>
            }
        > = []
        vi.stubGlobal(
            'Audio',
            class extends EventTarget {
                volume = 0
                pause = vi.fn()
                remove = vi.fn()
                play = vi.fn(() =>
                    audio.length === 1
                        ? new Promise<void>((_resolve, reject) => {
                              rejectOld = reject
                          })
                        : Promise.resolve(),
                )
                constructor() {
                    super()
                    audio.push(this)
                }
            },
        )
        document.body.innerHTML = '<div risu-ctrl="bgm___auto___fixture"></div>'
        stop = startObserveDom()
        stop()
        stop = startObserveDom()
        rejectOld(new Error('old audio interrupted'))
        await settle()
        document
            .querySelector('div')!
            .setAttribute('risu-ctrl', 'bgm___auto___fixture-next')
        await settle()
        expect(audio).toHaveLength(2)
        expect(audio[0].pause).toHaveBeenCalledOnce()
        expect(audio[1].remove).not.toHaveBeenCalled()
        audio[1].dispatchEvent(new Event('ended'))
        expect(audio).toHaveLength(3)
        window.dispatchEvent(new Event('pagehide'))
        expect(audio[2].pause).toHaveBeenCalledOnce()
    })
    it('binds retained code blocks once without polling the document', async () => {
        document.body.innerHTML = '<pre x-hl-lang="js">fixture</pre>'
        const result = startObserveDom()
        if (typeof result === 'function') stop = result
        const scan = vi.spyOn(document, 'querySelectorAll')
        const create = vi.spyOn(document, 'createElement')
        vi.useFakeTimers()
        await vi.advanceTimersByTimeAsync(500)
        openMenu(document.querySelector('pre')!)
        expect(create).toHaveBeenCalledTimes(3)
        expect(scan).not.toHaveBeenCalled()
    })

    it('observes nested additions and current language attributes', async () => {
        stop = startObserveDom()
        const wrapper = document.createElement('section')
        wrapper.innerHTML = '<div><pre>fixture</pre></div>'
        document.body.append(wrapper)
        const code = wrapper.querySelector('pre')!
        code.setAttribute('x-hl-lang', 'js')
        await settle()
        code.setAttribute('x-hl-lang', 'ts')
        await settle()
        openMenu(code)
        ;(
            document.querySelector('#code-contextmenu')!
                .lastChild as HTMLElement
        ).click()
        await settle()
        expect(download).toHaveBeenLastCalledWith(
            'code.ts',
            expect.any(Uint8Array),
        )
    })

    it('supports repeated startup, detach/reattach, and cleanup', async () => {
        stop = startObserveDom()
        expect(startObserveDom()).toBe(stop)
        const code = document.createElement('pre')
        code.setAttribute('x-hl-lang', 'js')
        document.body.append(code)
        await settle()
        code.remove()
        document.body.append(code)
        await settle()
        const create = vi.spyOn(document, 'createElement')
        openMenu(code)
        expect(create).toHaveBeenCalledTimes(3)
        stop()
        expect(document.getElementById('code-contextmenu')).toBeNull()
        openMenu(code)
        expect(create).toHaveBeenCalledTimes(3)
        stop = startObserveDom()
        openMenu(code)
        expect(create).toHaveBeenCalledTimes(6)
    })
})
