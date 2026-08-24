import { describe, expect, it, vi } from 'vitest'
import {
    installPersistentSaveNotifications,
    type PersistentRuntimeNotificationCallbacks,
} from './persistentSaveNotifications'

function makeChannel() {
    return {
        onmessage: null as ((event: MessageEvent) => void) | null,
        postMessage: vi.fn(),
        close: vi.fn(),
    }
}

describe('installPersistentSaveNotifications', () => {
    it('announces each local revision on the channel and reports the committed save', () => {
        const channel = makeChannel()
        const onSaveCommitted = vi.fn()
        let callbacks: PersistentRuntimeNotificationCallbacks = {}
        installPersistentSaveNotifications({
            sessionId: 'session-a',
            channel,
            configureRuntime: (value) => {
                callbacks = value
            },
            showForeignRevisionWarning: vi.fn(),
            setSaving: vi.fn(),
            onSaveCommitted,
        })

        callbacks.onLocalRevision?.(7)

        expect(channel.postMessage).toHaveBeenCalledWith('session-a')
        expect(onSaveCommitted).toHaveBeenCalledTimes(1)
    })

    it('warns once about a foreign revision and clears callbacks on dispose', () => {
        const channel = makeChannel()
        const showForeignRevisionWarning = vi.fn()
        let callbacks: PersistentRuntimeNotificationCallbacks = {}
        const dispose = installPersistentSaveNotifications({
            sessionId: 'session-a',
            channel,
            configureRuntime: (value) => {
                callbacks = value
            },
            showForeignRevisionWarning,
            setSaving: vi.fn(),
        })

        channel.onmessage?.({ data: 'session-a' } as MessageEvent)
        expect(showForeignRevisionWarning).not.toHaveBeenCalled()
        channel.onmessage?.({ data: 'session-b' } as MessageEvent)
        channel.onmessage?.({ data: 'session-b' } as MessageEvent)
        expect(showForeignRevisionWarning).toHaveBeenCalledTimes(1)

        dispose()
        expect(callbacks.onLocalRevision).toBeUndefined()
        expect(channel.close).toHaveBeenCalledTimes(1)
    })
})
