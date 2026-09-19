import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { SandboxHost } from './factory'

/**
 * The guest reports when its top level script has settled. A window the host
 * opened for that script must stay open until the calls the script already made
 * have answered.
 */
async function runGuest(host: SandboxHost, code: string): Promise<Window & typeof globalThis> {
    const frame = document.createElement('iframe')
    document.body.appendChild(frame)
    host.run(frame, code, 'settled-test')
    const child = frame.contentWindow as Window & typeof globalThis
    const childRealm = child as any
    Object.defineProperty(child, 'ImageBitmap', {
        configurable: true,
        value: globalThis.ImageBitmap,
    })
    vi.spyOn(window, 'postMessage').mockImplementation((data: unknown) => {
        window.dispatchEvent(new MessageEvent('message', { data, source: child }))
    })
    vi.spyOn(child.parent, 'postMessage').mockImplementation((data: unknown) => {
        window.dispatchEvent(new MessageEvent('message', { data, source: child }))
    })
    vi.spyOn(child, 'postMessage').mockImplementation(function (
        data: unknown,
        options?: WindowPostMessageOptions,
    ) {
        const legacyTransfer = arguments[2] as Transferable[] | undefined
        const transfer = options?.transfer ?? legacyTransfer ?? []
        child.dispatchEvent(new childRealm.MessageEvent('message', {
            data,
            source: window,
            ports: transfer as MessagePort[],
        }))
    })
    const source = frame.srcdoc.match(/<script nonce="[^"]+">([\s\S]*)<\/script>/)?.[1]
    if (!source) throw new Error('Sandbox guest script was not found')
    await childRealm.eval(source)
    return child
}

/** The guest bridge asks for these before it hands control to the script. */
function bridgeStubs(): Record<string, unknown> {
    return {
        _getPropertiesForInitialization: () => ({ apiVersion: '3.0', list: ['apiVersion'] }),
        _getAliases: () => ({}),
    }
}

describe('sandbox script completion', () => {
    beforeEach(() => {
        vi.stubGlobal('ImageBitmap', class {})
    })

    afterEach(() => {
        vi.restoreAllMocks()
        for (const frame of document.querySelectorAll('iframe')) frame.remove()
    })

    /** Invariant 24. */
    it('reports completion only after the calls the script made have answered', async () => {
        let release: (() => void) | null = null
        const pending = new Promise<string>((resolve) => {
            release = () => resolve('answered')
        })
        const settled = vi.fn()
        const slowCall = vi.fn(() => pending)
        const host = new SandboxHost({
            ...bridgeStubs(),
            slowCall,
        })
        host.onScriptSettled(settled)

        await runGuest(host, `window.answer = await risuai.slowCall()`)
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(slowCall).toHaveBeenCalledTimes(1)
        expect(settled).not.toHaveBeenCalled()

        release?.()
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(settled).toHaveBeenCalledTimes(1)
    })

    it('reports completion once even when the script throws', async () => {
        const settled = vi.fn()
        const host = new SandboxHost(bridgeStubs())
        host.onScriptSettled(settled)

        await runGuest(host, `throw new Error('plugin failed to start')`)
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(settled).toHaveBeenCalledTimes(1)
    })

    it('assembles a streamed snapshot as a plain iframe-owned object', async () => {
        const host = new SandboxHost({
            ...bridgeStubs(),
            streamedSnapshot: () => ({
                __type: 'IFRAME_OBJECT_STREAM',
                value: new ReadableStream({
                    start(controller) {
                        controller.enqueue({ type: 'arrayStart', key: 'characters' })
                        controller.enqueue({
                            type: 'arrayPush',
                            key: 'characters',
                            value: { chaId: 'a', chats: [] },
                        })
                        controller.enqueue({ type: 'set', key: 'username', value: 'Fixture' })
                        controller.enqueue({ type: 'recordStart', key: 'pluginCustomStorage' })
                        controller.enqueue({
                            type: 'recordSet',
                            key: 'pluginCustomStorage',
                            entryKey: 'constructor',
                            value: 'constructor-value',
                        })
                        controller.enqueue({
                            type: 'recordSet',
                            key: 'pluginCustomStorage',
                            entryKey: '__proto__',
                            value: 'prototype-value',
                        })
                        controller.close()
                    },
                }),
            }),
        })

        const child = await runGuest(host, `
            window.snapshot = await risuai.streamedSnapshot();
            window.snapshotIsPlain = window.snapshot.constructor === Object;
            window.snapshot.characters[0].name = 'mutated';
        `)
        await vi.waitFor(() => expect((child as any).snapshot).toBeDefined())
        expect((child as any).snapshotIsPlain).toBe(true)
        const snapshot = (child as any).snapshot
        expect(snapshot.characters).toEqual([{ chaId: 'a', chats: [], name: 'mutated' }])
        expect(snapshot.username).toBe('Fixture')
        expect(snapshot.pluginCustomStorage.constructor).toBe('constructor-value')
        expect(Object.hasOwn(snapshot.pluginCustomStorage, '__proto__')).toBe(true)
        expect(snapshot.pluginCustomStorage.__proto__).toBe('prototype-value')
    })
})
