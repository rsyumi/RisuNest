// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'

vi.mock('./persistentDataRuntime.svelte', () => ({
    flushPendingData: vi.fn(async () => undefined),
}))

import { registerLifecycleCommitListeners } from './lifecycleCommit'

const originalVisibilityState = Object.getOwnPropertyDescriptor(document, 'visibilityState')

function setVisibilityState(value: DocumentVisibilityState): void {
    Object.defineProperty(document, 'visibilityState', {
        configurable: true,
        value,
    })
}

afterEach(() => {
    if (originalVisibilityState) {
        Object.defineProperty(document, 'visibilityState', originalVisibilityState)
    } else {
        Reflect.deleteProperty(document, 'visibilityState')
    }
    vi.restoreAllMocks()
})

describe('registerLifecycleCommitListeners', () => {
    it('flushes every pagehide event', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true }))

        expect(flush).toHaveBeenCalledWith('pagehide')
        dispose()
    })

    it('flushes when the document becomes hidden', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)
        setVisibilityState('hidden')

        document.dispatchEvent(new Event('visibilitychange'))

        expect(flush).toHaveBeenCalledWith('visibility-hidden')
        dispose()
    })

    it('does not flush while the document is visible', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)
        setVisibilityState('visible')

        document.dispatchEvent(new Event('visibilitychange'))

        expect(flush).not.toHaveBeenCalled()
        dispose()
    })

    it.each(['stop', 'trim-memory'] as const)('forwards native %s events', (reason) => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason },
        }))

        expect(flush).toHaveBeenCalledWith(reason)
        dispose()
    })

    it.each([
        undefined,
        null,
        {},
        { reason: 'unknown' },
        { reason: 1 },
    ])('ignores malformed native lifecycle payload %#', (detail) => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail }))

        expect(flush).not.toHaveBeenCalled()
        dispose()
    })

    it('forwards close-together eligible events independently', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new Event('pagehide'))
        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'stop' },
        }))

        expect(flush.mock.calls).toEqual([['pagehide'], ['stop']])
        dispose()
    })

    it('removes all listeners and can be disposed twice', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)
        dispose()
        dispose()
        setVisibilityState('hidden')

        window.dispatchEvent(new Event('pagehide'))
        document.dispatchEvent(new Event('visibilitychange'))
        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'trim-memory' },
        }))

        expect(flush).not.toHaveBeenCalled()
    })
})
