// @vitest-environment happy-dom

import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

vi.mock('src/ts/stores.svelte', async () => ({
    alertStore: (await import('src/ts/alertQueue')).createAlertQueue({ type: 'none', msg: '' }, { gapMs: 0 }),
    DBState: {
        db: {
            zoomsize: 100,
            lineHeight: 1.25,
        },
    },
}))

import { language } from 'src/lang'
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

test('retains the original source and draft when an asynchronous partial save is refused', async () => {
    const saved = deferred<boolean>()
    const onSave = vi.fn(() => saved.promise)
    const windowLayer = document.createElement('div')
    document.body.append(windowLayer)
    mounted = mount(PartialEditControllerHarness, { target, props: {
        getTranslationEditContext: async () => null, onSave, windowLayer,
    } })
    ;(mounted as HarnessInstance).setTranslatedView(false)
    await tick()
    const root = target.querySelector('div')!
    const block = target.querySelector('p')!
    vi.spyOn(document, 'elementFromPoint').mockReturnValue(block)
    TestIntersectionObserver.instance?.setVisible(root)
    await tick()
    document.dispatchEvent(new MouseEvent('mousemove', { clientX: 10, clientY: 10 }))
    await tick()
    document.querySelector<HTMLButtonElement>('.partial-edit-btn-edit')!.click()
    await vi.waitFor(() => expect(document.querySelector('.partial-edit-modal textarea')).not.toBeNull())
    // The chat pane clips fixed layers inside it, so the dialog mounts in the chat screen's window layer.
    expect(windowLayer.contains(document.querySelector('.partial-edit-overlay'))).toBe(true)
    const input = document.querySelector<HTMLTextAreaElement>('.partial-edit-modal textarea')!
    input.value = 'Retained draft'
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await tick()
    document.querySelector<HTMLButtonElement>('.partial-edit-save-btn')!.click()
    await vi.waitFor(() => expect(onSave).toHaveBeenCalledOnce())
    expect(onSave).toHaveBeenCalledWith(expect.objectContaining({ sourceData: 'Shared text', newData: 'Retained draft' }))
    saved.resolve(false)
    await tick()
    await Promise.resolve()
    expect(document.querySelector('.partial-edit-modal')).not.toBeNull()
    expect(input.value).toBe('Retained draft')
})

test('mounts the match, delete and match failure dialogs in the chat screen window layer', async () => {
    // The hover handler hovers again only after its frame callback has run, so frames run after it returns.
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
        queueMicrotask(() => callback(0))
        return 1
    })
    const windowLayer = document.createElement('div')
    document.body.append(windowLayer)
    mounted = mount(PartialEditControllerHarness, { target, props: {
        getTranslationEditContext: async () => null, windowLayer, messageData: 'Shared text\n\nShared text',
    } })
    ;(mounted as HarnessInstance).setTranslatedView(false)
    await tick()
    const block = target.querySelector('p')!
    vi.spyOn(document, 'elementFromPoint').mockReturnValue(block)
    TestIntersectionObserver.instance?.setVisible(target.querySelector('div')!)
    await tick()
    const overlayInLayer = () => {
        const overlays = document.querySelectorAll('.partial-edit-overlay')
        return overlays.length === 1 && windowLayer.contains(overlays[0])
    }
    const press = async (button: string) => {
        document.dispatchEvent(new MouseEvent('mousemove', { clientX: 10, clientY: 10 }))
        await tick()
        document.querySelector<HTMLButtonElement>(button)!.click()
        await tick()
    }

    // The block appears twice in the source, so deleting it asks which one first.
    await press('.partial-edit-btn-delete')
    await vi.waitFor(() => expect(document.querySelector('.match-item')).not.toBeNull())
    expect(overlayInLayer()).toBe(true)
    document.querySelector<HTMLElement>('.match-item')!.click()
    await vi.waitFor(() => expect(document.querySelector('.match-item')).toBeNull())
    expect(overlayInLayer()).toBe(true)
    document.querySelector<HTMLButtonElement>('.partial-edit-cancel-btn')!.click()
    await vi.waitFor(() => expect(document.querySelector('.partial-edit-overlay')).toBeNull())

    block.textContent = 'Quiz 4096'
    await press('.partial-edit-btn-edit')
    await vi.waitFor(() => expect(document.querySelector('.partial-edit-overlay')).not.toBeNull())
    expect(overlayInLayer()).toBe(true)
    expect(document.querySelector('.partial-edit-overlay')?.textContent).toContain(language.partialEdit.matchFailedTitle)
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
