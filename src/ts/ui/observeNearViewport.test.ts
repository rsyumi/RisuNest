import { afterEach, describe, expect, it, vi } from 'vitest'

class TestIntersectionObserver {
    static instances: TestIntersectionObserver[] = []

    readonly observed = new Set<Element>()
    readonly unobserve = vi.fn((element: Element) =>
        this.observed.delete(element),
    )
    readonly disconnect = vi.fn(() => this.observed.clear())

    constructor(
        private readonly callback: IntersectionObserverCallback,
        readonly options?: IntersectionObserverInit,
    ) {
        TestIntersectionObserver.instances.push(this)
    }

    observe(element: Element) {
        this.observed.add(element)
    }

    setVisible(...elements: Element[]) {
        this.callback(
            elements.map(
                (target) =>
                    ({
                        target,
                        isIntersecting: true,
                    }) as IntersectionObserverEntry,
            ),
            this as unknown as IntersectionObserver,
        )
    }
}

afterEach(() => {
    TestIntersectionObserver.instances = []
    vi.unstubAllGlobals()
    vi.resetModules()
})

describe('observeNearViewport', () => {
    it('shares one observer and invokes only intersecting elements once', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const { observeNearViewport } = await import('./observeNearViewport')
        const first = document.createElement('div')
        const second = document.createElement('div')
        const firstCallback = vi.fn()
        const secondCallback = vi.fn()

        const stopFirst = observeNearViewport(first, firstCallback)
        const stopSecond = observeNearViewport(second, secondCallback)
        const observer = TestIntersectionObserver.instances[0]

        expect(TestIntersectionObserver.instances).toHaveLength(1)
        expect(observer.options).toMatchObject({
            root: null,
            rootMargin: '240px',
            threshold: 0,
        })
        observer.setVisible(first)
        observer.setVisible(first)

        expect(firstCallback).toHaveBeenCalledTimes(1)
        expect(secondCallback).not.toHaveBeenCalled()
        expect(observer.unobserve).toHaveBeenCalledWith(first)

        stopFirst()
        stopSecond()
        expect(observer.disconnect).toHaveBeenCalledTimes(1)
    })

    it('removes a pending callback during cleanup', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const { observeNearViewport } = await import('./observeNearViewport')
        const element = document.createElement('div')
        const callback = vi.fn()

        const stop = observeNearViewport(element, callback)
        const observer = TestIntersectionObserver.instances[0]
        stop()
        observer.setVisible(element)

        expect(callback).not.toHaveBeenCalled()
        expect(observer.disconnect).toHaveBeenCalledTimes(1)
    })

    it('loads immediately when IntersectionObserver is unavailable', async () => {
        vi.stubGlobal('IntersectionObserver', undefined)
        const { observeNearViewport } = await import('./observeNearViewport')
        const callback = vi.fn()

        const stop = observeNearViewport(
            document.createElement('div'),
            callback,
        )

        expect(callback).toHaveBeenCalledTimes(1)
        expect(() => stop()).not.toThrow()
    })
})
