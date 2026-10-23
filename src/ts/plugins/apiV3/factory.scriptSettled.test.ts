import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { SandboxHost, UNREAD_PLUGIN_STREAM_TIMEOUT_MS } from './factory'
import { invoke } from '@tauri-apps/api/core'
import { consoleLog } from '../../consoleLog'
const platform = vi.hoisted(() => ({ native: false }))
vi.mock('../../platform', async () => (await import('../../platform.testSupport')).platformModule(() => ({ runtime: platform.native ? 'tauri' : 'web', os: 'windows' })))
vi.mock('@tauri-apps/api/core', async importOriginal => ({
    ...await importOriginal<typeof import('@tauri-apps/api/core')>(), invoke: vi.fn(async () => {}),
}))
import { collectPluginReadProvenance, PluginReadBaselines } from '../pluginReadBaselines'
import { SafeLocalPluginStorage } from '../pluginSafeClass'
import { PluginDeviceKeyspace, type PluginDeviceBackend, type PluginDeviceValue } from '../pluginDeviceKeyspace'
import type { fetchNative, GlobalFetchArgs } from '../../globalApi.svelte'

/**
 * The guest reports when its top level script has settled. A window the host
 * opened for that script must stay open until the calls the script already made
 * have answered.
 */
async function runGuest(host: SandboxHost, code: string, prepare?: (child: Window & typeof globalThis) => void, cloneMessages = false): Promise<Window & typeof globalThis> {
    const frame = document.createElement('iframe')
    document.body.appendChild(frame)
    host.run(frame, code, 'settled-test')
    const child = frame.contentWindow as Window & typeof globalThis
    const childRealm = child as any
    Object.defineProperty(child, 'ImageBitmap', {
        configurable: true,
        value: globalThis.ImageBitmap,
    })
    Object.defineProperty(child, 'structuredClone', { configurable: true, value: globalThis.structuredClone })
    const deliverToHost = function(data: unknown, _origin?: unknown) {
        const transfer = arguments[2] as Transferable[] | undefined
        const received = cloneMessages ? structuredClone(data, { transfer }) : data
        window.dispatchEvent(new MessageEvent('message', { data: received, source: child }))
    }
    vi.spyOn(window, 'postMessage').mockImplementation(deliverToHost)
    vi.spyOn(child.parent, 'postMessage').mockImplementation(deliverToHost)
    vi.spyOn(child, 'postMessage').mockImplementation(function (
        data: unknown,
        options?: WindowPostMessageOptions,
    ) {
        const legacyTransfer = arguments[2] as Transferable[] | undefined
        const transfer = options?.transfer ?? legacyTransfer ?? []
        child.dispatchEvent(new childRealm.MessageEvent('message', {
            data: cloneMessages ? structuredClone(data, { transfer }) : data,
            source: child.parent,
            ports: (cloneMessages ? transfer.filter(item => item instanceof MessagePort) : transfer) as MessagePort[],
        }))
    })
    const source = frame.srcdoc.match(/<script nonce="[^"]+">([\s\S]*)<\/script>/)?.[1]
    if (!source) throw new Error('Sandbox guest script was not found')
    prepare?.(child)
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

function localStorageFixture(beforeWrite: () => Promise<void> = async () => {}) {
    const rows = new Map<string, PluginDeviceValue>()
    const backend: PluginDeviceBackend = {
        async hydrate() { return { complete: true, byteSize: 0, entries: [...rows].map(([key, value]) => ({ space: 'json', key, value })) } },
        async read(_owner, _space, key) { return rows.get(key) ?? null },
        async keys() { return [...rows.keys()] },
        async write(_owner, mutations) {
            await beforeWrite()
            for (const mutation of mutations) {
                if (mutation.type === 'set') rows.set(mutation.key, mutation.value)
                else if (mutation.type === 'delete') rows.delete(mutation.key)
                else rows.clear()
            }
        },
    }
    const keyspace = new PluginDeviceKeyspace('rpc-json', backend)
    return { rows, keyspace, storage: new SafeLocalPluginStorage('rpc-json', keyspace) }
}

describe('sandbox script completion', () => {
    beforeEach(() => consoleLog.clear())

    it('sends large local JSON arrays as text and keeps their stored representation and write ordering', async () => {
        let started!: () => void, release!: () => void
        const admitted = new Promise<void>(resolve => { started = resolve })
        const gate = new Promise<void>(resolve => { release = resolve })
        let first = true
        const fixture = localStorageFixture(async () => { if (first) { first = false; started(); await gate } })
        const result = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), getLocalPluginStorage: () => fixture.storage, result })
        try {
            const child = await runGuest(host, `
                const storage = await risuai.getLocalPluginStorage();
                const large = Array.from({ length: 1024 }, (_, id) => ({ id, text: 'entry', optional: undefined, nan: NaN }));
                const first = storage.setItem('value', large);
                const second = storage.setItem('value', { last: true });
                await first;
                await second;
                await risuai.result(await storage.getItem('value'));
            `, undefined, true)
            await admitted
            const calls = vi.mocked(child.parent.postMessage).mock.calls.map(([data]) => data as any)
            const request = calls.find(data => data.method === 'setItem' && typeof data.localPluginJson === 'string')
            expect(request).toMatchObject({ args: ['value'] })
            expect(JSON.parse(request.localPluginJson)).toEqual(Array.from({ length: 1024 }, (_, id) => ({ id, text: 'entry', nan: null })))
            release()
            await vi.waitFor(() => expect(result).toHaveBeenCalledWith({ last: true }))
            expect(fixture.rows.get('value')).toBe('{"last":true}')
        } finally { release(); host.terminate(); fixture.keyspace.dispose() }
    })

    it('retains binary values when an array getter changes between preflight and the native snapshot', async () => {
        const fixture = localStorageFixture()
        const result = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), getLocalPluginStorage: () => fixture.storage, result })
        try {
            await runGuest(host, `
                const storage = await risuai.getLocalPluginStorage();
                const value = Array(1024).fill(null);
                const bytes = new Uint8Array([0, 255, 128]);
                let reads = 0;
                Object.defineProperty(value, '0', { enumerable: true, get: () => ++reads === 1 ? 'plain' : bytes });
                await storage.setItem('value', value);
                const restored = await storage.getItem('value');
                await risuai.result({ binary: ArrayBuffer.isView(restored[0]), bytes: Array.from(restored[0]) });
            `, undefined, true)
            await vi.waitFor(() => expect(result).toHaveBeenCalledWith({ binary: true, bytes: [0, 255, 128] }))
            expect(fixture.rows.get('value')).toBeInstanceOf(Uint8Array)
        } finally { host.terminate(); fixture.keyspace.dispose() }
    })

    it('rejects malformed serialized JSON from the frame without replacing a stored value', async () => {
        const fixture = localStorageFixture()
        await fixture.storage.setItem('value', 'kept')
        const host = new SandboxHost({})
        const frame = document.createElement('iframe')
        document.body.append(frame)
        host.run(frame, '')
        const internals = host as any
        internals.instanceRegistry.set('local-storage', fixture.storage)
        const post = vi.spyOn(frame.contentWindow!, 'postMessage').mockImplementation(() => {})
        try {
            window.dispatchEvent(new MessageEvent('message', { source: frame.contentWindow, data: {
                type: 'CALL_INSTANCE', id: 'local-storage', method: 'setItem', reqId: 'invalid-json',
                args: ['value'], localPluginJson: '{invalid',
            } }))
            await vi.waitFor(() => expect(post.mock.calls.some(([data]) => data.reqId === 'invalid-json' && typeof data.error === 'string')).toBe(true))
            fixture.keyspace.invalidate()
            expect(await fixture.storage.getItem('value')).toBe('kept')
            expect(fixture.rows.get('value')).toBe('"kept"')
        } finally { host.terminate(); fixture.keyspace.dispose() }
    })

    it('round-trips cyclic binary graphs and shared backing buffers across both iframe transfers', async () => {
        const result = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), binaryEcho: (value: unknown) => structuredClone(value), result })
        await runGuest(host, `
            const buffer = new Uint8Array([0, 255, 128, 3]).buffer;
            const value = { buffer, view: new Uint8Array(buffer, 1, 2) };
            value.self = value;
            const restored = await risuai.binaryEcho(value);
            await risuai.result({
                cyclic: restored.self === restored,
                shared: restored.view.buffer === restored.buffer,
                offset: restored.view.byteOffset,
                bytes: Array.from(restored.view),
            });
        `, undefined, true)
        expect(result).toHaveBeenCalledWith({ cyclic: true, shared: true, offset: 1, bytes: [255, 128] })
        host.terminate()
    })

    it.each([false, true])('lets the plugin handle file-read failure without reporting it on native=%s', async native => {
        platform.native = native
        vi.mocked(invoke).mockClear()
        const alertError = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), alertError })
        const child = await runGuest(host, `
            const reader = new FileReader();
            reader.onerror = () => { globalThis.readError = reader.error.name; };
            reader.readAsArrayBuffer(new File(['synthetic'], 'synthetic.risup'));
        `, child => {
            vi.spyOn(child.FileReader.prototype, 'readAsArrayBuffer').mockImplementation(function () {
                Object.defineProperty(this, 'error', { value: new child.DOMException('Provider denied access', 'NotReadableError') })
                this.dispatchEvent(new child.Event('error'))
            })
        })
        await vi.waitFor(() => expect((child as any).readError).toBe('NotReadableError'))
        expect(alertError).not.toHaveBeenCalled()
        expect(consoleLog.snapshot().some(entry => entry.level === 'error')).toBe(false)
        expect(invoke).not.toHaveBeenCalledWith('native_log_error', expect.anything())
        host.terminate()
    })

    it('preserves a handled promise-based read failure without reporting it', async () => {
        const alertError = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), alertError })
        const failure = new DOMException('File disappeared', 'NotFoundError')
        const child = await runGuest(host, `
            try { await new File(['synthetic'], 'synthetic.risup').arrayBuffer(); }
            catch (error) { globalThis.readError = error.message; }
        `, child => {
            vi.spyOn(child.Blob.prototype, 'arrayBuffer').mockRejectedValue(failure)
        })
        await vi.waitFor(() => expect((child as any).readError).toBe('File disappeared'))
        expect(alertError).not.toHaveBeenCalled()
        expect(consoleLog.snapshot().some(entry => entry.level === 'error')).toBe(false)
        host.terminate()
    })

    it.each([false, true])('records top-level failures and causes without a popup on native=%s', async native => {
        platform.native = native
        vi.mocked(invoke).mockClear()
        const alertError = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), alertError })
        await runGuest(host, `throw new Error('Failed to read file', {cause: new DOMException('Provider failed', 'NotReadableError')});`)
        await vi.waitFor(() => expect(consoleLog.snapshot()).toContainEqual(expect.objectContaining({
            level: 'error', source: 'settled-test', message: 'Error: Failed to read file\nNotReadableError: Provider failed',
        })))
        expect(alertError).not.toHaveBeenCalled()
        const logged = { message: '[settled-test] Error: Failed to read file\nNotReadableError: Provider failed' }
        if (native) expect(invoke).toHaveBeenCalledWith('native_log_error', logged)
        else expect(invoke).not.toHaveBeenCalledWith('native_log_error', logged)
        expect(host.isIdle()).toBe(true)
        host.terminate()
    })

    it.each([
        'Object.create(null)',
        'new Proxy({}, { get() { throw new Error("getter failed"); } })',
        'Object.defineProperty(new Error("original"), "cause", { get() { throw new Error("cause getter failed"); } })',
    ])('settles even when error reporting cannot stringify %s', async (expression) => {
        const alertError = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), alertError })
        await runGuest(host, `throw ${expression};`)
        await vi.waitFor(() => expect(host.isIdle()).toBe(true))
        expect(consoleLog.snapshot().filter(entry => entry.level === 'error')).toHaveLength(1)
        expect(alertError).not.toHaveBeenCalled()
        host.terminate()
    })

    it('accepts console entries only from its live plugin frame with the host-owned name', () => {
        const alertError = vi.fn()
        const host = new SandboxHost({ alertError })
        const frame = document.createElement('iframe')
        document.body.append(frame)
        host.run(frame, '', 'file-plugin')
        const tsMs = 0
        const data = { type: 'PLUGIN_CONSOLE', generation: consoleLog.generation, source: 'forged', entries: [{ level: 'log', message: 'synthetic', tsMs }] }
        window.dispatchEvent(new MessageEvent('message', { source: window, data }))
        expect(consoleLog.snapshot()).toEqual([])
        const source = frame.contentWindow
        window.dispatchEvent(new MessageEvent('message', { source, data }))
        expect(consoleLog.snapshot()).toEqual([{ level: 'log', source: 'file-plugin', message: 'synthetic', tsMs }])
        host.terminate()
        window.dispatchEvent(new MessageEvent('message', { source, data }))
        expect(consoleLog.snapshot()).toHaveLength(1)
        expect(alertError).not.toHaveBeenCalled()
    })

    it('captures ordinary iframe output at every level and still allows explicit alerts', async () => {
        const alertError = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), alertError })
        await runGuest(host, `
            for (const level of ['log', 'info', 'debug', 'warn', 'error']) console[level]('synthetic-' + level, { count: 3 });
            await risuai.alertError('explicit alert');
        `)
        await vi.waitFor(() => expect(host.isIdle()).toBe(true))
        for (const level of ['log', 'info', 'debug', 'warn', 'error']) {
            expect(consoleLog.snapshot()).toContainEqual(expect.objectContaining({
                level, source: 'settled-test', message: 'synthetic-' + level + ' {count: 3}',
            }))
        }
        expect(alertError).toHaveBeenCalledWith('explicit alert')
        host.terminate()
        expect(consoleLog.snapshot().some(entry => entry.message.startsWith('synthetic-log'))).toBe(true)
    })

    it('discards a delayed iframe batch from before clear and keeps later output', () => {
        const host = new SandboxHost(bridgeStubs())
        const frame = document.createElement('iframe')
        document.body.append(frame)
        host.run(frame, '', 'clear-test')
        const post = vi.spyOn(frame.contentWindow!, 'postMessage')
        const beforeClear = consoleLog.generation
        consoleLog.clear()
        post.mockClear()
        window.dispatchEvent(new MessageEvent('message', { source: frame.contentWindow, data: {
            type: 'PLUGIN_CONSOLE', generation: beforeClear, entries: [{ level: 'log', message: 'old', tsMs: 0 }],
        } }))
        expect(post).toHaveBeenCalledWith({ type: 'CLEAR_CONSOLE', generation: consoleLog.generation }, '*')
        window.dispatchEvent(new MessageEvent('message', { source: frame.contentWindow, data: {
            type: 'PLUGIN_CONSOLE', generation: consoleLog.generation, entries: [{ level: 'info', message: 'new', tsMs: 0 }],
        } }))
        expect(consoleLog.snapshot().map(entry => entry.message)).toEqual(['new'])
        host.terminate()
    })

    it('keeps the end of an iframe log burst and reports omitted entries', async () => {
        const host = new SandboxHost(bridgeStubs())
        await runGuest(host, `for (let n = 0; n < 201; n++) console.log('burst-' + n);`, child => {
            vi.spyOn(child.console, 'log').mockImplementation(() => {})
        })
        await vi.waitFor(() => expect(host.isIdle()).toBe(true))
        expect(consoleLog.snapshot()).toContainEqual(expect.objectContaining({ message: 'burst-200' }))
        expect(consoleLog.snapshot().some(entry => entry.message === 'burst-0')).toBe(false)
        expect(consoleLog.snapshot().some(entry => entry.message.includes('console messages omitted'))).toBe(true)
        expect(consoleLog.snapshot().length).toBeLessThanOrEqual(200)
        host.terminate()
    })

    it('stays busy after a callback returns until its stream finishes', async () => {
        let callback!: () => Promise<{content: ReadableStream}>
        const host = new SandboxHost({ register: (fn: typeof callback) => { callback = fn } })
        const frame = document.createElement('iframe')
        document.body.append(frame)
        host.run(frame, '')
        const post = vi.spyOn(frame.contentWindow!, 'postMessage')
        const send = (data: unknown, ports: MessagePort[] = []) => window.dispatchEvent(new MessageEvent('message', {
            source: frame.contentWindow, data, ports,
        }))
        send({type:'CALL_ROOT', method:'register', reqId:'register', args:[{__type:'CALLBACK_REF', id:'stream-callback'}]})
        send({type:'SCRIPT_SETTLED'})
        await vi.waitFor(() => expect(host.isIdle()).toBe(true))
        const result = callback()
        expect(host.isIdle()).toBe(false)
        const invocation = post.mock.calls.at(-1)![0]
        const channel = new MessageChannel()
        send({type:'CALLBACK_RETURN', reqId:invocation.reqId, result:{content:{__type:'STREAM_PORT', portIndex:0}}}, [channel.port2])
        const {content} = await result
        expect(host.isIdle()).toBe(false)
        const reader = content.getReader()
        channel.port1.postMessage({done:true})
        expect(await reader.read()).toEqual({done:true, value:undefined})
        expect(host.isIdle()).toBe(true)
        channel.port1.close()
        host.terminate()
    })

    it('counts a delivered stream as busy only once the plugin reads it', async () => {
        let finish!: (stream: ReadableStream) => void
        const host = new SandboxHost({ ...bridgeStubs(), stream: () => new Promise(resolve => { finish = resolve }) })
        const changed = vi.fn()
        host.onActivityChanged(changed)
        const child = await runGuest(host, `globalThis.result = risuai.stream()`)
        await vi.waitFor(() => expect(finish).toBeTypeOf('function'))
        expect(host.isIdle()).toBe(false)
        const cancel = vi.fn()
        finish(new ReadableStream({ cancel }))
        const stream = await (child as any).result
        await vi.waitFor(() => expect(host.isIdle()).toBe(true))
        const reader = stream.getReader()
        const pending = reader.read()
        await vi.waitFor(() => expect(host.isIdle()).toBe(false))
        await reader.cancel()
        expect(await pending).toEqual({ done: true, value: undefined })
        await vi.waitFor(() => expect(host.isIdle()).toBe(true))
        expect(cancel).toHaveBeenCalledOnce()
        expect(changed).toHaveBeenCalled()
        host.terminate()
    })

    it('releases an unread response without blocking plugin reload and rejects a late read', async () => {
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        const cancel = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), nativeFetch: () => ({ status: 200, body: new ReadableStream({ cancel }) }) })
        try {
            const child = await runGuest(host, `globalThis.response = await risuai.nativeFetch('https://synthetic.invalid')`)
            await vi.waitFor(() => expect((child as any).response?.status).toBe(200))
            expect(host.isIdle()).toBe(true)
            await vi.advanceTimersByTimeAsync(UNREAD_PLUGIN_STREAM_TIMEOUT_MS)
            expect(cancel).toHaveBeenCalledOnce()
            await expect((child as any).response.body.getReader().read()).rejects.toThrow('Unread plugin response expired')
        } finally {
            host.terminate()
            vi.useRealTimers()
        }
    })

    it('keeps a consumed response alive through backpressure and cancels it on unload', async () => {
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        const cancel = vi.fn()
        const host = new SandboxHost({ ...bridgeStubs(), stream: () => new ReadableStream({ cancel }) })
        try {
            const child = await runGuest(host, `globalThis.response = await risuai.stream()`)
            await vi.waitFor(() => expect((child as any).response).toBeDefined())
            const reader = (child as any).response.getReader()
            const pending = reader.read()
            await vi.waitFor(() => expect(host.isIdle()).toBe(false))
            await vi.advanceTimersByTimeAsync(UNREAD_PLUGIN_STREAM_TIMEOUT_MS * 2)
            expect(cancel).not.toHaveBeenCalled()
            expect(host.isIdle()).toBe(false)
            host.terminate()
            await reader.cancel()
            await pending
            expect(cancel).toHaveBeenCalledOnce()
            expect(vi.getTimerCount()).toBe(0)
        } finally {
            host.terminate()
            vi.useRealTimers()
        }
    })

    // Each method reads the signal field of the host function it reaches.
    it.each([
        ['nativeFetch', 'signal' satisfies keyof Parameters<typeof fetchNative>[1]],
        ['risuFetch', 'abortSignal' satisfies keyof GlobalFetchArgs],
    ] as const)('aborts an admitted %s with and without a caller signal', async (method, field) => {
        for (const callerSignal of [false, true]) {
            let signal: AbortSignal | undefined
            const fetcher = vi.fn((_url, options) => new Promise(resolve => {
                signal = options[field]
                signal!.addEventListener('abort', () => resolve(null), { once: true })
            }))
            const host = new SandboxHost({ ...bridgeStubs(), [method]: fetcher })
            const frame = document.createElement('iframe')
            document.body.append(frame)
            host.run(frame, '')
            window.dispatchEvent(new MessageEvent('message', {
                source: frame.contentWindow,
                data: {type:'CALL_ROOT', method, reqId:'synthetic-fetch', args:['https://synthetic.invalid', callerSignal
                    ? {[field]:{__type:'ABORT_SIGNAL_REF', abortId:'caller', aborted:false}} : {}]},
            }))
            await vi.waitFor(() => expect(fetcher).toHaveBeenCalledOnce())
            expect(signal?.aborted).toBe(false)
            host.terminate()
            expect(signal?.aborted).toBe(true)
            await vi.waitFor(() => expect(host.isIdle()).toBe(true))
        }
    })

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

    it('ignores malformed parent envelopes while a legitimate response still completes', async () => {
        let resolve!: (value: string) => void
        const host = new SandboxHost({
            ...bridgeStubs(),
            held: () => new Promise<string>(done => { resolve = done }),
        })
        const child = await runGuest(host, 'window.answer = await risuai.held()')
        const request = vi.mocked(child.parent.postMessage).mock.calls
            .map(call => call[0] as any).find(data => data.method === 'held')
        expect(request).toBeDefined()
        const childRealm = child as any
        for (const data of [
            null, [], 'RESPONSE',
            { type: 'RESPONSE', reqId: request.reqId, error: { message: 'forged' } },
            { type: 'RESPONSE', reqId: 1, result: 'forged' },
            { type: 'EXECUTE_CODE', reqId: 'malformed-code', code: 1 },
            { type: 'INVOKE_CALLBACK', reqId: 'malformed-callback', id: 'x', args: {} },
            { type: 'ABORT_SIGNAL', abortId: 1 },
        ]) {
            child.dispatchEvent(new childRealm.MessageEvent('message', { source: child.parent, data }))
        }
        await Promise.resolve()
        expect((child as any).answer).toBeUndefined()
        expect(vi.mocked(child.parent.postMessage).mock.calls.map(call => call[0]))
            .not.toEqual(expect.arrayContaining([expect.objectContaining({ type: 'EXEC_RESULT' })]))
        resolve('real')
        await vi.waitFor(() => expect((child as any).answer).toBe('real'))
        host.terminate()
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

    it('transports nested read tokens separately from product values in both directions', async () => {
        const registry = new PluginReadBaselines('synthetic-plugin', () => 1)
        const value = registry.track({ characters: [{ chaId: 'a', name: 'Character', chats: [{ id: 'chat', name: 'Before', message: [] }] }] }, 'database', 'database', 1)
        const write = vi.fn((chat: any) => registry.intent(chat, 'conversation', '["a","chat"]', {}))
        const host = new SandboxHost({ ...bridgeStubs(), getDatabase: () => value, setChatToIndex: (_characterIndex: number, _chatIndex: number, chat: any) => write(chat) })
        const child = await runGuest(host, `
            const database = await risuai.getDatabase();
            window.productKeys = Object.keys(database.characters[0].chats[0]);
            database.characters[0].chats[0].name = 'Edited';
            await risuai.setChatToIndex(0, 0, database.characters[0].chats[0]);
            window.finishedWrite = true;
        `)
        await vi.waitFor(() => expect((child as any).finishedWrite).toBe(true))
        expect(write.mock.results[0].value).toEqual([{ path: ['name'], type: 'set', value: 'Edited' }])
        expect((child as any).productKeys).toEqual(['id', 'name', 'message'])
        const sent = vi.mocked(child.parent.postMessage).mock.calls.map(call => call[0] as any).find(message => message.method === 'setChatToIndex')
        expect(sent.argProvenance[2]).toEqual([{ path: [], token: collectPluginReadProvenance(value.characters[0].chats[0])[0].token }])
        expect(sent.args[2]).not.toHaveProperty('token')
        host.terminate()
    })

    it('keeps read tokens next to byte arrays without visiting each byte', async () => {
        const registry = new PluginReadBaselines('synthetic-plugin', () => 1)
        const value = registry.track({ characters: [{ chaId: 'a', name: 'Character', chats: [{ id: 'chat', name: 'Before', message: [] }] }] }, 'database', 'database', 1)
        const chat = value.characters[0].chats[0]
        const saved = vi.fn()
        const host = new SandboxHost({
            ...bridgeStubs(),
            readSyntheticImage: () => ({ bytes: new Uint8Array([17, 29, 43]), chat }),
            saveSyntheticImage: saved,
        })
        const child = await runGuest(host, `
            const entries = Object.entries;
            Object.entries = function (value) {
                if (ArrayBuffer.isView(value)) throw new Error('Byte arrays must not be enumerated');
                return entries(value);
            };
            const read = await risuai.readSyntheticImage();
            await risuai.saveSyntheticImage(read.bytes, read.chat);
            window.finishedWrite = true;
        `)
        await vi.waitFor(() => expect((child as any).finishedWrite).toBe(true))
        expect(Array.from(saved.mock.calls[0][0])).toEqual([17, 29, 43])
        const sent = vi.mocked(child.parent.postMessage).mock.calls.map(call => call[0] as any).find(message => message.method === 'saveSyntheticImage')
        expect(sent.argProvenance).toEqual([[], [{ path: [], token: collectPluginReadProvenance(chat)[0].token }]])
        host.terminate()
    })

    it('associates final streamed provenance with the selected character and nested chat', async () => {
        const registry = new PluginReadBaselines('synthetic-plugin', () => 1)
        const value = { characters: [registry.track({ chaId: 'a', name: 'Character', chats: [{ id: 'chat', name: 'Before', message: [] }] }, 'character', 'a', 1)] }
        const write = vi.fn((chat: any) => registry.intent(chat, 'conversation', '["a","chat"]', {}))
        const host = new SandboxHost({
            ...bridgeStubs(),
            getCharacter: () => ({ __type: 'IFRAME_OBJECT_STREAM', select: 'character', value: new ReadableStream({
                start(controller) {
                    controller.enqueue({ type: 'arrayStart', key: 'characters' })
                    controller.enqueue({ type: 'arrayPush', key: 'characters', value: value.characters[0] })
                    controller.enqueue({ type: 'provenance', entries: collectPluginReadProvenance(value) })
                    controller.close()
                },
            }) }),
            setChatToIndex: (_character: number, _chat: number, chat: any) => write(chat),
        })
        const child = await runGuest(host, `
            const character = await risuai.getCharacter();
            character.chats[0].name = 'Stream edit';
            await risuai.setChatToIndex(0, 0, character.chats[0]);
            window.finishedWrite = true;
        `)
        await vi.waitFor(() => expect((child as any).finishedWrite).toBe(true))
        expect(write.mock.results[0].value).toEqual([{ path: ['name'], type: 'set', value: 'Stream edit' }])
        host.terminate()
    })

    it('drains admitted storage mutations after termination and refuses late ingress', async () => {
        let finish!: () => void
        const pending = new Promise<void>(resolve => { finish = resolve })
        const mutation = vi.fn(() => pending)
        const host = new SandboxHost({ ...bridgeStubs(), _setSafeLocalStorage: mutation })
        const child = await runGuest(host, `void risuai._setSafeLocalStorage('synthetic', 'value')`)
        await vi.waitFor(() => expect(mutation).toHaveBeenCalledOnce())
        host.terminate()
        const drained = vi.fn()
        const draining = host.drainStorageMutations().then(drained)
        await Promise.resolve()
        expect(drained).not.toHaveBeenCalled()
        window.dispatchEvent(new MessageEvent('message', { source: child, data: { type: 'CALL_ROOT', reqId: 'late-write', method: '_setSafeLocalStorage', args: ['late', 'value'] } }))
        expect(mutation).toHaveBeenCalledOnce()
        finish()
        await draining
        expect(drained).toHaveBeenCalledOnce()
    })
})

describe('sandbox result lifetime and policy', () => {
    afterEach(() => {
        vi.restoreAllMocks()
        for (const frame of document.querySelectorAll('iframe')) frame.remove()
    })

    it('keeps the opaque sandbox and restrictive CSP', () => {
        const host = new SandboxHost({})
        const frame = document.createElement('iframe')
        document.body.append(frame)
        host.run(frame, '')
        const nonce = frame.srcdoc.match(/<script nonce="([^"]+)">/)![1]
        expect(frame.getAttribute('sandbox')).toBe('allow-scripts allow-modals allow-downloads')
        expect(frame.getAttribute('csp')).toBe("connect-src 'none'; script-src 'nonce-" + nonce + "' 'wasm-unsafe-eval'; frame-src 'none'; object-src 'none'; style-src * 'unsafe-inline'; default-src 'none'; img-src * data: blob:; font-src * data: blob:; media-src * data: blob:; base-uri 'none';")
        host.terminate()
    })

    it.each(['root', 'instance'])('disposes a late %s result without registering streams or remote refs', async (kind) => {
        let resolve!: (value: unknown) => void
        const call = vi.fn(() => new Promise(done => { resolve = done }))
        const host = new SandboxHost({ call })
        const frame = document.createElement('iframe')
        document.body.append(frame)
        host.run(frame, '')
        const internals = host as any
        internals.instanceRegistry.set('instance', { call })
        const post = vi.spyOn(frame.contentWindow!, 'postMessage')
        window.dispatchEvent(new MessageEvent('message', {
            source: frame.contentWindow,
            data: { type: kind === 'root' ? 'CALL_ROOT' : 'CALL_INSTANCE', id: 'instance', reqId: 'late', method: 'call', args: [] },
        }))
        expect(call).toHaveBeenCalledTimes(1)
        host.terminate()
        const cancel = vi.fn()
        resolve({ value: new ReadableStream({ cancel }), __classType: 'REMOTE_REQUIRED' })
        await vi.waitFor(() => expect(internals.pendingHostCalls).toBe(0))
        expect(cancel).toHaveBeenCalledTimes(1)
        expect(internals.activeStreamCleanups.size).toBe(0)
        expect(internals.instanceRegistry.size).toBe(0)
        expect(post).not.toHaveBeenCalled()
        host.terminate()
        expect(cancel).toHaveBeenCalledTimes(1)
    })

    it('cancels an undeliverable response body', async () => {
        let resolve!: (value: unknown) => void
        const host = new SandboxHost({ call: () => new Promise(done => { resolve = done }) })
        const frame = document.createElement('iframe')
        document.body.append(frame)
        host.run(frame, '')
        window.dispatchEvent(new MessageEvent('message', {
            source: frame.contentWindow,
            data: { type: 'CALL_ROOT', reqId: 'late', method: 'call', args: [] },
        }))
        const cancel = vi.fn()
        frame.remove()
        resolve(new Response(new ReadableStream({ cancel })))
        await vi.waitFor(() => expect((host as any).pendingHostCalls).toBe(0))
        expect(cancel).toHaveBeenCalledTimes(1)
        expect((host as any).activeStreamCleanups.size).toBe(0)
        host.terminate()
    })
})
