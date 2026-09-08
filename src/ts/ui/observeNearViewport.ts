type NearViewportCallback = () => void

const callbacks = new Map<Element, NearViewportCallback>()
let observer: IntersectionObserver | null = null

function getObserver(): IntersectionObserver {
    if (observer) return observer

    observer = new IntersectionObserver(
        (entries) => {
            for (const entry of entries) {
                if (!entry.isIntersecting) continue

                const callback = callbacks.get(entry.target)
                if (!callback) continue

                callbacks.delete(entry.target)
                observer?.unobserve(entry.target)
                callback()
            }

            releaseObserverIfIdle()
        },
        {
            root: null,
            rootMargin: '240px',
            threshold: 0,
        },
    )
    return observer
}

function releaseObserverIfIdle() {
    if (callbacks.size !== 0 || !observer) return
    observer.disconnect()
    observer = null
}

export function observeNearViewport(
    element: Element,
    callback: NearViewportCallback,
): () => void {
    if (typeof IntersectionObserver === 'undefined') {
        callback()
        return () => {}
    }

    const previous = callbacks.get(element)
    if (previous) observer?.unobserve(element)

    callbacks.set(element, callback)
    getObserver().observe(element)

    let active = true
    return () => {
        if (!active) return
        active = false

        if (callbacks.get(element) !== callback) return
        callbacks.delete(element)
        observer?.unobserve(element)
        releaseObserverIfIdle()
    }
}
