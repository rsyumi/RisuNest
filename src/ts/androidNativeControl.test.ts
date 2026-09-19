import { afterEach, describe, expect, it, vi } from 'vitest'
import { createAndroidControlClient, type AndroidControlMessagePort } from './androidNativeControl'

function fixture() {
    const sent: { id?: string; method: string; args: string[] }[] = []
    const port: AndroidControlMessagePort = {
        postMessage: (text) => { sent.push(JSON.parse(text)) },
    }
    const client = createAndroidControlClient(port)
    const reply = (value: unknown) => port.onmessage?.({ data: JSON.stringify(value) })
    return { sent, port, client, reply }
}

afterEach(() => {
    vi.useRealTimers()
    delete window.RisuNestControl
    delete window.RisuNestSafControl
    delete window.RisuLifecycleBridge
    delete window.RisuGenerationKeepAlive
    delete window.RisuSafBridge
})

describe('scoped Android control transport', () => {
    it.each([false, true])('announces the real app document after installing controls (SAF %s)', async (safEnabled) => {
        vi.resetModules()
        const ready = vi.fn((text: string) => {
            expect(window.RisuLifecycleBridge?.onFlushComplete).toBeTypeOf('function')
            expect(window.RisuGenerationKeepAlive?.begin).toBeTypeOf('function')
            expect(Boolean(window.RisuSafBridge)).toBe(safEnabled)
            expect(JSON.parse(text)).toEqual({ method: 'lifecycle.onFrontendReady', args: [] })
        })
        window.RisuNestControl = { postMessage: ready }
        if (safEnabled) window.RisuNestSafControl = { postMessage: vi.fn() }
        await import('./androidNativeControl')
        expect(ready).toHaveBeenCalledOnce()
    })

    it('matches delayed responses without treating promises as successful booleans', async () => {
        const { client, sent, reply } = fixture()
        const first = client.request<boolean>('saf.acknowledgeExport', 'first')
        const second = client.request<boolean>('saf.acknowledgeExport', 'second')
        expect(sent.map(({ method, args }) => ({ method, args }))).toEqual([
            { method: 'saf.acknowledgeExport', args: ['first'] },
            { method: 'saf.acknowledgeExport', args: ['second'] },
        ])
        reply({ id: sent[1].id, result: false })
        reply({ id: sent[0].id, result: true })
        expect(await first).toBe(true)
        expect(await second).toBe(false)
    })

    it('keeps exit holds synchronous to send and independent of pending requests', () => {
        const { client, sent } = fixture()
        client.notify('lifecycle.onFlushHold', 'exit-1')
        expect(sent).toEqual([{ method: 'lifecycle.onFlushHold', args: ['exit-1'] }])
    })

    it('rejects native errors and ignores unrelated or malformed replies', async () => {
        const { client, sent, reply, port } = fixture()
        const pending = client.request<string>('saf.getExportStatus')
        const rejected = expect(pending).rejects.toThrow('android-control-failed')
        port.onmessage?.({ data: '{' })
        reply(null)
        reply({ id: 'unrelated', result: 'ignored' })
        reply({ id: sent[0].id, error: 'android-control-failed' })
        await rejected
    })

    it('bounds pending requests and never retries an uncertain native action', async () => {
        vi.useFakeTimers()
        const { client, sent } = fixture()
        const pending = Array.from({ length: 32 }, () =>
            client.request<boolean>('saf.acknowledgeExport', 'receipt').catch((error: Error) => error.message),
        )
        await expect(client.request('saf.getExportStatus')).rejects.toThrow('android-control-busy')
        await vi.advanceTimersByTimeAsync(15_000)
        expect(await Promise.all(pending)).toEqual(Array(32).fill('android-control-timeout'))
        expect(sent).toHaveLength(32)
        expect(vi.getTimerCount()).toBe(0)
    })

    it('releases pending state when sending fails', async () => {
        vi.useFakeTimers()
        const port: AndroidControlMessagePort = { postMessage: () => { throw new Error('closed') } }
        const client = createAndroidControlClient(port)
        await expect(client.request('generation.begin')).rejects.toThrow('closed')
        expect(vi.getTimerCount()).toBe(0)
    })
})
