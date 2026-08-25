import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
}))

import { fetch as tauriFetch } from '@tauri-apps/plugin-http'

describe('installed Tauri HTTP upload contract', () => {
    beforeEach(() => {
        mocks.invoke.mockReset()
        mocks.invoke.mockImplementation(async (command: string) => {
            if (command === 'plugin:http|fetch') return 1
            if (command === 'plugin:http|fetch_send') {
                return {
                    status: 204,
                    statusText: 'No Content',
                    url: 'https://account.invalid/write',
                    headers: [],
                    rid: 2,
                }
            }
            return undefined
        })
        ;(window as any).__TAURI_INTERNALS__ = { invoke: mocks.invoke }
    })

    afterEach(() => {
        delete (window as any).__TAURI_INTERNALS__
    })

    it('drains a request stream into one byte array before starting native fetch', async () => {
        const body = new ReadableStream<Uint8Array>({
            start(controller) {
                controller.enqueue(Uint8Array.of(1, 2))
                controller.enqueue(Uint8Array.of(3, 4))
                controller.close()
            },
        })

        await tauriFetch('https://account.invalid/write', {
            method: 'POST',
            body,
            duplex: 'half',
        } as RequestInit)

        expect(mocks.invoke.mock.calls[0][0]).toBe('plugin:http|fetch')
        expect(mocks.invoke.mock.calls[0][1]).toEqual(expect.objectContaining({
            clientConfig: expect.objectContaining({ data: [1, 2, 3, 4] }),
        }))
    })

    it('encodes a native export path as text instead of reading its file bytes', async () => {
        const path = 'C:\\app\\persistent\\exports\\snapshot.risudat'

        await tauriFetch('https://account.invalid/write', {
            method: 'POST',
            body: path,
        })

        expect(mocks.invoke.mock.calls[0][1].clientConfig.data).toEqual(
            [...new TextEncoder().encode(path)],
        )
    })
})
