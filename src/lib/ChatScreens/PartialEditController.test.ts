// @vitest-environment happy-dom

import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

vi.mock('src/ts/stores.svelte', () => ({
    DBState: {
        db: {
            zoomsize: 100,
            lineHeight: 1.25,
        },
    },
}))

import PartialEditControllerHarness from './PartialEditControllerHarness.test.svelte'

type HarnessInstance = {
    setTranslatedView(value: boolean): void
    getSaves(): Array<{
        newData: string
        target: 'original' | 'translation'
        translationKey?: string
    }>
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

    setVisible(element: Element) {
        this.callback([{
            target: element,
            isIntersecting: true,
            intersectionRatio: 1,
        } as IntersectionObserverEntry], this as unknown as IntersectionObserver)
    }
}

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((done) => { resolve = done })
    return { promise, resolve }
}

let mounted: ReturnType<typeof mount> | undefined
let target: HTMLDivElement

beforeEach(() => {
    target = document.createElement('div')
    document.body.append(target)
    vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
        callback(0)
        return 1
    })
    vi.stubGlobal('cancelAnimationFrame', vi.fn())
})

afterEach(async () => {
    if (mounted) await unmount(mounted)
    mounted = undefined
    TestIntersectionObserver.instance = undefined
    document.body.replaceChildren()
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
})

test('abandons a deferred translation partial edit when the translated view changes', async () => {
    const translationContext = deferred<{ key: string; data: string } | null>()
    const getTranslationEditContext = vi.fn(() => translationContext.promise)
    mounted = mount(PartialEditControllerHarness, {
        target,
        props: { getTranslationEditContext },
    })
    await tick()

    const bodyRoot = target.querySelector('div')!
    const translatedBlock = target.querySelector('p')!
    vi.spyOn(document, 'elementFromPoint').mockReturnValue(translatedBlock)
    TestIntersectionObserver.instance?.setVisible(bodyRoot)
    await tick()
    document.dispatchEvent(new MouseEvent('mousemove', { clientX: 10, clientY: 10 }))
    await tick()
    const staleEditButton = document.querySelector<HTMLButtonElement>('.partial-edit-btn-edit')!
    staleEditButton.click()
    await vi.waitFor(() => expect(getTranslationEditContext).toHaveBeenCalledOnce())

    const harness = mounted as HarnessInstance
    harness.setTranslatedView(false)
    await tick()
    translationContext.resolve({ key: 'translation-key', data: 'Shared text' })
    await Promise.resolve()
    await Promise.resolve()
    await Promise.resolve()
    await tick()

    const firstCompletionModalOpened = document.querySelector('.partial-edit-modal') !== null
    const controlsHidden = staleEditButton.closest<HTMLElement>('.partial-edit-btn-wrapper')?.style.display === 'none'
    staleEditButton.click()
    await Promise.resolve()
    await Promise.resolve()
    await tick()
    const secondClickModalOpened = document.querySelector('.partial-edit-modal') !== null
    document.querySelector<HTMLButtonElement>('.partial-edit-save-btn')?.click()
    await tick()

    expect({
        firstCompletionModalOpened,
        controlsHidden,
        contextRequests: getTranslationEditContext.mock.calls.length,
        secondClickModalOpened,
        saves: harness.getSaves(),
    }).toEqual({
        firstCompletionModalOpened: false,
        controlsHidden: true,
        contextRequests: 1,
        secondClickModalOpened: false,
        saves: [],
    })
})

test('abandons a deferred translation partial edit when its rendered block detaches', async () => {
    const translationContext = deferred<{ key: string; data: string } | null>()
    const getTranslationEditContext = vi.fn(() => translationContext.promise)
    mounted = mount(PartialEditControllerHarness, {
        target,
        props: { getTranslationEditContext },
    })
    await tick()

    const bodyRoot = target.querySelector('div')!
    const translatedBlock = target.querySelector('p')!
    vi.spyOn(document, 'elementFromPoint').mockReturnValue(translatedBlock)
    TestIntersectionObserver.instance?.setVisible(bodyRoot)
    await tick()
    document.dispatchEvent(new MouseEvent('mousemove', { clientX: 10, clientY: 10 }))
    await tick()
    const staleEditButton = document.querySelector<HTMLButtonElement>('.partial-edit-btn-edit')!
    staleEditButton.click()
    await vi.waitFor(() => expect(getTranslationEditContext).toHaveBeenCalledOnce())

    translatedBlock.remove()
    translationContext.resolve({ key: 'translation-key', data: 'Shared text' })
    await Promise.resolve()
    await Promise.resolve()
    await Promise.resolve()
    await tick()

    const firstCompletionModalOpened = document.querySelector('.partial-edit-modal') !== null
    const controlsHidden = staleEditButton.closest<HTMLElement>('.partial-edit-btn-wrapper')?.style.display === 'none'
    staleEditButton.click()
    await Promise.resolve()
    await Promise.resolve()
    await tick()
    const secondClickModalOpened = document.querySelector('.partial-edit-modal') !== null
    document.querySelector<HTMLButtonElement>('.partial-edit-save-btn')?.click()
    await tick()

    expect({
        firstCompletionModalOpened,
        controlsHidden,
        contextRequests: getTranslationEditContext.mock.calls.length,
        secondClickModalOpened,
        saves: (mounted as HarnessInstance).getSaves(),
    }).toEqual({
        firstCompletionModalOpened: false,
        controlsHidden: true,
        contextRequests: 1,
        secondClickModalOpened: false,
        saves: [],
    })
})
