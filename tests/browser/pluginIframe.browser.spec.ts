import { inflateSync } from 'node:zlib'
import { readFile } from 'node:fs/promises'
import type { Page } from '@playwright/test'
import { test, expect } from './fixture'

// A single-pixel PNG row decodes to its raw bytes under every filter type.
async function pixelAt(page: Page, x: number, y: number) {
    const png = await page.screenshot({ clip: { x, y, width: 1, height: 1 } })
    const data: Buffer[] = []
    for (let offset = 8; offset < png.length; offset += png.readUInt32BE(offset) + 12) {
        if (png.toString('ascii', offset + 4, offset + 8) === 'IDAT') data.push(png.subarray(offset + 8, offset + 8 + png.readUInt32BE(offset)))
    }
    return [...inflateSync(Buffer.concat(data)).subarray(1, 4)]
}

test('a fullscreen frame shows the host through its transparent areas on a dark color scheme', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => {
        document.documentElement.style.colorScheme = 'dark'
        const backdrop = document.createElement('div')
        backdrop.style.cssText = 'position:fixed;inset:0;background:rgb(12,34,56)'
        document.body.append(backdrop)
    })
    await page.evaluate(() => window.boundary.load(`
        const mark = document.createElement('div');
        mark.style.cssText = 'position:fixed;left:0;top:0;width:10px;height:10px;background:rgb(200,0,0)';
        document.body.append(mark);
        await risuai.showContainer('fullscreen');
        await risuai.pluginStorage.setItem('shown', true);
    `))
    await expect.poll(() => page.evaluate(() => window.boundary.state.writes)).toEqual([{ key: 'shown', value: true }])
    await expect(page.locator('[data-risu-plugin-frame]')).toBeVisible()
    expect(await pixelAt(page, 5, 5)).toEqual([200, 0, 0])
    expect(await pixelAt(page, 40, 40)).toEqual([12, 34, 56])
})

test('real iframe clones payloads and delivers responses asynchronously', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => window.boundary.load(`
        const payload = { text: 'sent' };
        const order = [];
        const pending = risuai.pluginStorage.setItem('payload', payload);
        payload.text = 'mutated'; order.push('same-turn');
        await pending; order.push('response');
        await risuai.pluginStorage.setItem('order', order);
    `))
    await expect.poll(() => page.evaluate(() => window.boundary.state.writes)).toEqual([
        { key: 'payload', value: { text: 'sent' } }, { key: 'order', value: ['same-turn', 'response'] },
    ])
    await expect(page.locator('[data-risu-plugin-frame]')).toHaveAttribute('sandbox', 'allow-scripts allow-modals allow-downloads')
})

const canvasEncoderCases = ['toBlob', 'convertToBlob'].flatMap(method => [false, true].map(forceFallback => ({ method, forceFallback })))
for (const { method, forceFallback } of canvasEncoderCases) {
    test(`native plugin ${method} returns real WebP while preserving the canvas snapshot with PNG fallback=${forceFallback}`, async ({ page }) => {
        const encoded = [...await readFile(new URL('../fixtures/images/still.webp', import.meta.url))]
        await page.addInitScript(({ bytes, forceFallback }) => {
            if (window.parent !== window) return
            if (forceFallback) {
                const srcdoc = Object.getOwnPropertyDescriptor(HTMLIFrameElement.prototype, 'srcdoc')!
                // WebKit does not run Playwright init scripts in srcdoc frames. Install the
                // unsupported-encoder boundary before the real guest bridge in both engines.
                const installPngFallback = function() {
                    const toBlob = HTMLCanvasElement.prototype.toBlob
                    HTMLCanvasElement.prototype.toBlob = function(callback, type, quality) {
                        return toBlob.call(this, callback, String(type).toLowerCase() === 'image/webp' ? 'image/png' : type, quality)
                    }
                    if (typeof OffscreenCanvas !== 'undefined') {
                        const convertToBlob = OffscreenCanvas.prototype.convertToBlob
                        OffscreenCanvas.prototype.convertToBlob = function(options) {
                            return convertToBlob.call(this, String(options?.type).toLowerCase() === 'image/webp' ? { ...options, type: 'image/png' } : options)
                        }
                    }
                }
                Object.defineProperty(HTMLIFrameElement.prototype, 'srcdoc', {
                    ...srcdoc,
                    set(html: string) {
                        srcdoc.set!.call(this, html.replace(/(<script nonce="[^"]+">)/, opening => `${opening}(${installPngFallback.toString()})();`))
                    },
                })
            }
            const requests: unknown[] = []
            Object.assign(window, { canvasEncodeRequests: requests, __TAURI_INTERNALS__: {
                metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
                invoke: async (command: string, args: { data: number[]; width: number; height: number; quality: number }) => {
                    if (command !== 'native_media_encode_canvas_webp') throw new Error(`Unexpected native command: ${command}`)
                    requests.push(args)
                    return { data: bytes, outputSize: bytes.length, metadata: { mime: 'image/webp', size: bytes.length, width: args.width, height: args.height } }
                },
            } })
        }, { bytes: encoded, forceFallback })
        await page.goto('/')
        const browserType = await page.evaluate(async method => {
            if (method === 'convertToBlob') {
                if (typeof OffscreenCanvas === 'undefined') return null
                const canvas = new OffscreenCanvas(4, 3)
                canvas.getContext('2d')
                return (await canvas.convertToBlob({ type: 'image/webp' })).type
            }
            return (await new Promise<Blob | null>(resolve => document.createElement('canvas').toBlob(resolve, 'image/webp')))!.type
        }, method)
        test.skip(browserType === null, 'This WebKit runner has no OffscreenCanvas')
        await page.evaluate(method => window.boundary.loadLocal(`
            const canvas = ${method === 'toBlob' ? "document.createElement('canvas')" : 'new OffscreenCanvas(4, 3)'};
            canvas.width = 4; canvas.height = 3;
            const context = canvas.getContext('2d');
            context.fillStyle = 'rgba(0,128,255,0.5)'; context.fillRect(0,0,4,3);
            const pending = ${method === 'toBlob' ? "new Promise(resolve => canvas.toBlob(resolve, 'image/webp', 0.85))" : "canvas.convertToBlob({ type: 'image/webp', quality: 0.85 })"};
            context.fillStyle = 'red'; context.fillRect(0,0,4,3);
            const blob = await pending;
            if (!blob || blob.type !== 'image/webp') throw new Error('이 브라우저는 WebP 저장을 지원하지 않습니다.');
            const decoded = await createImageBitmap(blob);
            await risuai.result({ type: blob.type, width: decoded.width, height: decoded.height, nativeExposed: '__TAURI_INTERNALS__' in window });
            decoded.close();
        `), method)
        await expect.poll(() => page.evaluate(() => window.boundary.localResult)).toEqual({ type: 'image/webp', width: 4, height: 3, nativeExposed: false })
        const requests = await page.evaluate(() => (window as any).canvasEncodeRequests)
        if (!forceFallback && browserType === 'image/webp') expect(requests).toEqual([])
        else expect(requests).toEqual([{ data: Array.from({ length: 12 }, () => [0, 128, 255, 128]).flat(), width: 4, height: 3, quality: 0.85 }])
    })
}

test('local plugin storage keeps structured clone values across the real iframe bridge', async ({ page }) => {
    // The native device store is a process boundary; parts live in this page.
    await page.addInitScript(() => {
        const parts = new Map<string, Uint8Array>()
        const uploads = new Map<string, number[]>()
        let next = 0
        Object.assign(window, { __TAURI_INTERNALS__: {
            metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
            transformCallback: () => ++next,
            invoke: async (command: string, args: { uploadId: string; data: number[]; hash: string; offset: number }) => {
                if (command === 'native_asset_bytes_probe') return false
                if (command === 'pds_open_plugin_device_part') { const id = `upload-${++next}`; uploads.set(id, []); return id }
                if (command === 'pds_append_plugin_device_part') { const bytes = uploads.get(args.uploadId)!; bytes.push(...args.data); return bytes.length }
                if (command === 'pds_finish_plugin_device_part') {
                    const bytes = Uint8Array.from(uploads.get(args.uploadId)!)
                    const hash = [...new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))].map(byte => byte.toString(16).padStart(2, '0')).join('')
                    parts.set(hash, bytes)
                    return hash
                }
                if (command === 'pds_read_plugin_device_part') return parts.get(args.hash)!.slice(args.offset, args.offset + 1024 * 1024).buffer
                return null
            },
        } })
    })
    await page.goto('/')
    await page.evaluate(() => window.boundary.loadLocal(`
        const storage = await risuai.getLocalPluginStorage();
        const buffer = new Uint8Array([0, 255, 128]).buffer;
        const map = new Map([['view', new Uint8Array(buffer, 1, 2)], ['buffer', buffer]]);
        map.set('self', map);
        await storage.setItem('value', { map, when: new Date(7), big: 5n, nan: NaN, negativeZero: -0, holes: [1, , 3],
            missing: undefined, set: new Set(['a']), blob: new Blob(['hi'], { type: 'text/plain' }) });
        await storage.setItem('plain', { a: [1] });
        const shared = new Uint8Array([1, 2, 3, 4]).buffer;
        const graph = { views: [new Uint8Array(shared, 0, 2), new Uint8Array(shared, 2, 2)], byRoot: new Map(), nan: NaN };
        graph.self = graph;
        graph.list = [graph];
        graph.byRoot.set('root', graph);
        await storage.setItem('graph', graph);
        const back = await storage.getItem('graph');
        const restored = await storage.getItem('value');
        const view = restored.map.get('view');
        await risuai.result({
            cycle: restored.map.get('self') === restored.map, shared: view.buffer === restored.map.get('buffer'),
            bytes: Array.from(view), time: restored.when instanceof Date ? restored.when.getTime() : null,
            big: restored.big === 5n, nan: Number.isNaN(restored.nan), negativeZero: Object.is(restored.negativeZero, -0),
            hole: restored.holes.length === 3 && !(1 in restored.holes), missing: 'missing' in restored && restored.missing === undefined,
            set: restored.set instanceof Set && restored.set.has('a'), blob: [restored.blob.type, await restored.blob.text()],
            plain: await storage.getItem('plain'),
            graph: [back.self === back, back.list[0] === back, back.byRoot.get('root') === back,
                back.views[0].buffer === back.views[1].buffer, Array.from(back.views[1]), Number.isNaN(back.nan)],
        });
    `))
    await expect.poll(() => page.evaluate(() => window.boundary.localResult)).toEqual({
        cycle: true, shared: true, bytes: [255, 128], time: 7, big: true, nan: true, negativeZero: true,
        hole: true, missing: true, set: true, blob: ['text/plain', 'hi'], plain: { a: [1] },
        graph: [true, true, true, true, [3, 4], true],
    })
    const rows = await page.evaluate(() => window.boundary.localRows)
    expect(rows.find(row => row.key === 'plain')?.value).toBe('{"a":[1]}')
    expect(rows.find(row => row.key === 'value')?.value.startsWith('#risunest-structured:1\n')).toBe(true)
})

const callbackScript = `
try {
    window.addEventListener('message', event => {
        if (event.data.type === 'INVOKE_CALLBACK') parent.postMessage({fixture:'callback', reqId:event.data.reqId}, '*');
    });
    await risuai.addTTSPreprocessor(() => new Promise(resolve => {
        const release = event => {
            if (!event.data.fixtureRelease) return;
            window.removeEventListener('message', release); resolve('real');
        };
        window.addEventListener('message', release);
    }));
} catch (error) { parent.postMessage({fixture:'error', error:String(error)}, '*'); }
`

test('wrong-source reply is ignored and the real callback still resolves', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(script => window.boundary.load(script), callbackScript)
    await expect.poll(() => page.evaluate(() => ({ size: window.boundary.state.callbacks.size, events: window.boundary.events }))).toMatchObject({ size: 1 })
    await page.evaluate(() => window.boundary.call())
    await expect.poll(() => page.evaluate(() => window.boundary.events.length)).toBe(1)
    await page.evaluate(() => window.boundary.forge((window.boundary.events[0] as { reqId: string }).reqId))
    await expect.poll(() => page.evaluate(() => window.boundary.events.length)).toBe(2)
    expect(await page.evaluate(() => window.boundary.callbackResult)).toBeNull()
    await page.evaluate(() => window.boundary.release())
    await expect.poll(() => page.evaluate(() => window.boundary.callbackResult)).toEqual({ value: 'real' })
})

test('unload rejects an owned pending callback and the next frame works', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(script => window.boundary.load(script), callbackScript)
    await expect.poll(() => page.evaluate(() => ({ size: window.boundary.state.callbacks.size, events: window.boundary.events }))).toMatchObject({ size: 1 })
    await page.evaluate(() => window.boundary.call())
    await expect.poll(() => page.evaluate(() => window.boundary.events.length)).toBe(1)
    const oldId = await page.evaluate(() => (window.boundary.events[0] as { reqId: string }).reqId)
    await page.evaluate(() => window.boundary.unload())
    await expect.poll(() => page.evaluate(() => window.boundary.callbackResult)).toEqual({ error: 'Error: Plugin sandbox terminated' })
    expect(await page.evaluate(() => window.boundary.released)).toBe(true)
    await page.evaluate(script => window.boundary.load(script), callbackScript)
    await expect.poll(() => page.evaluate(() => ({ size: window.boundary.state.callbacks.size, events: window.boundary.events }))).toMatchObject({ size: 1 })
    await page.evaluate(() => window.boundary.call())
    await page.evaluate(id => window.boundary.forge(id), oldId)
    await expect.poll(() => page.evaluate(() => window.boundary.events.length)).toBe(3)
    expect(await page.evaluate(() => window.boundary.callbackResult)).toBeNull()
    await page.evaluate(() => window.boundary.release())
    await expect.poll(() => page.evaluate(() => window.boundary.callbackResult)).toEqual({ value: 'real' })
})

test('a late host response cannot resume an unloaded plugin or affect its replacement', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => window.boundary.load(`
        const value = await risuai.pluginStorage.getItem('held');
        await risuai.pluginStorage.setItem('old-result', value);
    `))
    await expect.poll(() => page.evaluate(() => !!window.boundary.state.releaseHeld)).toBe(true)
    await page.evaluate(() => window.boundary.unload())
    await page.evaluate(() => window.boundary.load(`
        const value = await risuai.pluginStorage.getItem('current');
        await risuai.pluginStorage.setItem('new-result', value);
    `))
    await expect.poll(() => page.evaluate(() => window.boundary.state.writes)).toEqual([{ key: 'new-result', value: 'current-value' }])
    await page.evaluate(() => window.boundary.releaseHeld())
    await expect.poll(() => page.evaluate(() => window.boundary.oldCallsFinished)).toBe(true)
    expect(await page.evaluate(() => window.boundary.state.writes)).toEqual([{ key: 'new-result', value: 'current-value' }])
})

test('the guest ignores sibling responses, callbacks, aborts and code execution', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => window.boundary.load([
        "window.addEventListener('message', event => { if (event.data?.forged) parent.postMessage({ fixture: 'forgery-observed' }, '*'); });",
        "await risuai.addTTSPreprocessor(async signal => {",
        "  await risuai.pluginStorage.setItem('callback-started', true);",
        "  await new Promise(resolve => signal.addEventListener('abort', resolve, { once: true }));",
        "  await risuai.pluginStorage.setItem('callback-aborted', true);",
        "});",
        "const value = await risuai.pluginStorage.getItem('held');",
        "await risuai.pluginStorage.setItem('held-result', value);",
    ].join('\n')))
    await expect.poll(() => page.evaluate(() => !!window.boundary.state.releaseHeld)).toBe(true)
    await page.evaluate(() => {
        const messages = window.boundary.bridgeMessages
        const callback = messages.find(message => message.method === 'addTTSPreprocessor').args[0].id
        const held = messages.find(message => message.method === '_getPluginStorage' && message.args[0] === 'held').reqId
        document.querySelector<HTMLIFrameElement>('iframe[data-risu-plugin-frame]')!.contentWindow!.postMessage({
            type: 'INVOKE_CALLBACK', id: callback, reqId: 'parent-callback',
            args: [{ __type: 'ABORT_SIGNAL_REF', abortId: 'parent-abort', aborted: false }],
        }, '*')
        window.boundary.forgeGuest([
            { type: 'RESPONSE', reqId: held, result: 'forged', forged: true },
            { type: 'INVOKE_CALLBACK', id: callback, reqId: 'forged-callback', args: [], forged: true },
            { type: 'ABORT_SIGNAL', abortId: 'parent-abort', forged: true },
            { type: 'EXECUTE_CODE', reqId: 'forged-exec', code: "window.forged = true", forged: true },
        ])
    })
    await expect.poll(() => page.evaluate(() => window.boundary.events.filter((event: any) => event.fixture === 'forgery-observed').length)).toBe(4)
    await expect.poll(() => page.evaluate(() => window.boundary.state.writes)).toEqual([{ key: 'callback-started', value: true }])
    expect(await page.evaluate(() => window.boundary.bridgeMessages.filter(message => ['forged-callback', 'forged-exec'].includes(message.reqId)))).toEqual([])
    await page.evaluate(() => {
        window.boundary.releaseHeld()
        document.querySelector<HTMLIFrameElement>('iframe[data-risu-plugin-frame]')!.contentWindow!.postMessage({ type: 'ABORT_SIGNAL', abortId: 'parent-abort' }, '*')
    })
    await expect.poll(() => page.evaluate(() => window.boundary.state.writes)).toEqual(expect.arrayContaining([
        { key: 'held-result', value: 'late-old-value' }, { key: 'callback-aborted', value: true },
    ]))
})

test('production CSP and opaque origin block network, string compilation and parent storage access', async ({ page, baseURL }) => {
    await page.goto('/')
    await page.evaluate(origin => window.boundary.load([
        "const attempts = {",
        ` network: () => fetch('${origin}/'),`,
        " eval: () => eval('1'),",
        " function: () => new Function('return 1')(),",
        " parent: () => parent.document,",
        " storage: () => localStorage.getItem('synthetic'),",
        "};",
        "for (const [key, attempt] of Object.entries(attempts)) {",
        " let blocked = false; try { await attempt(); } catch { blocked = true; }",
        " await risuai.pluginStorage.setItem(key, blocked);",
        "}",
    ].join('\n')), new URL(baseURL!).origin)
    await expect.poll(() => page.evaluate(() => window.boundary.state.writes)).toEqual([
        { key: 'network', value: true }, { key: 'eval', value: true },
        { key: 'function', value: true }, { key: 'parent', value: true }, { key: 'storage', value: true },
    ])
})

test('native download compatibility saves detached Blob anchors before immediate URL revocation', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => window.boundary.loadDownload(`
        const file = new File([new Uint8Array([0, 255, 13, 10])], 'original.bin');
        const a = document.createElement('a');
        a.href = URL.createObjectURL(file);
        a.download = '한글.charx';
        a.click();
        URL.revokeObjectURL(a.href);
    `))
    await expect.poll(() => page.evaluate(() => window.boundary.downloads.files)).toEqual([{ name: '한글.charx', bytes: [0, 255, 13, 10] }])
    await expect.poll(() => page.evaluate(() => window.boundary.downloadIdle)).toBe(true)
    expect(await page.evaluate(() => window.boundary.downloads.errors)).toEqual([])
})

test('native compatibility handles real user clicks and data URLs in the opaque iframe', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => window.boundary.loadDownload(`
        const a = document.createElement('a');
        a.href = 'data:application/octet-stream;base64,AP8NCg==';
        a.download = 'export.risup';
        a.textContent = 'Save';
        document.body.append(a);
    `))
    await page.frameLocator('[data-risu-plugin-frame]').getByText('Save', { exact: true }).click()
    await expect.poll(() => page.evaluate(() => window.boundary.downloads.files)).toEqual([{ name: 'export.risup', bytes: [0, 255, 13, 10] }])
    await expect(page.locator('[data-risu-plugin-frame]')).toHaveAttribute('sandbox', 'allow-scripts allow-modals allow-downloads')
})

test('native compatibility preserves prevented clicks and ordinary links', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => window.boundary.loadDownload(`
        const a = document.createElement('a');
        a.href = 'data:text/plain,blocked';
        a.download = 'blocked.txt';
        a.onclick = event => event.preventDefault();
        a.click();
        document.body.append(a);
        a.click();
        const ordinary = document.createElement('a');
        ordinary.href = 'data:text/plain,ordinary';
        document.body.append(ordinary);
        window.addEventListener('click', event => {
            if (!event.composedPath().includes(ordinary)) return;
            parent.postMessage({ fixture: 'ordinary-link', prevented: event.defaultPrevented }, '*');
            event.preventDefault();
        });
        ordinary.click();
        document.body.dataset.done = 'yes';
    `))
    await expect(page.frameLocator('[data-risu-plugin-frame]').locator('body')).toHaveAttribute('data-done', 'yes')
    await expect.poll(() => page.evaluate(() => window.boundary.downloadIdle)).toBe(true)
    expect(await page.evaluate(() => window.boundary.downloads.files)).toEqual([])
    expect(await page.evaluate(() => window.boundary.events)).toContainEqual({ fixture: 'ordinary-link', prevented: false })
})

test('native compatibility accepts FileSaver-style non-bubbling dispatched clicks', async ({ page }) => {
    const engineDownloads: string[] = []
    page.on('download', download => engineDownloads.push(download.suggestedFilename()))
    await page.goto('/')
    await page.evaluate(() => window.boundary.loadDownload(`
        for (const attached of [false, true]) {
            const a = document.createElement('a');
            a.href = URL.createObjectURL(new Blob(['dispatched']));
            a.download = attached ? 'attached.txt' : 'detached.txt';
            if (attached) document.body.append(a);
            a.dispatchEvent(new MouseEvent('click'));
            URL.revokeObjectURL(a.href);
        }
    `))
    await expect.poll(() => page.evaluate(() => window.boundary.downloads.files)).toEqual([
        { name: 'detached.txt', bytes: [...Buffer.from('dispatched')] },
        { name: 'attached.txt', bytes: [...Buffer.from('dispatched')] },
    ])
    expect(engineDownloads).toEqual([])
})

test('another frame cannot submit a native download', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => {
        window.boundary.loadDownload('document.body.dataset.ready = "yes"')
        const other = document.createElement('iframe')
        other.sandbox.add('allow-scripts')
        other.srcdoc = `<script>parent.postMessage({type:'CALL_ROOT',reqId:'forged-download',method:'_downloadFile',args:['forged.txt','data:text/plain,forged']},'*');parent.postMessage({fixture:'download-forgery-sent'},'*')<\/script>`
        document.body.append(other)
    })
    await expect.poll(() => page.evaluate(() => window.boundary.events)).toContainEqual({ fixture: 'download-forgery-sent' })
    await expect(page.frameLocator('[data-risu-plugin-frame]').locator('body')).toHaveAttribute('data-ready', 'yes')
    expect(await page.evaluate(() => window.boundary.downloads.files)).toEqual([])
})

test('native compatibility delivers HTTP bytes and reports failed downloads', async ({ page, baseURL }) => {
    await page.route('**/synthetic-download.bin', route => route.fulfill({ body: Buffer.from([0, 255, 7]), contentType: 'application/octet-stream' }))
    await page.goto('/')
    await page.evaluate(url => window.boundary.loadDownload(`
        const a = document.createElement('a');
        a.href = ${JSON.stringify(url)};
        a.download = 'network.bin';
        document.body.append(a);
        a.click();
    `), new URL('/synthetic-download.bin', baseURL).href)
    await expect.poll(() => page.evaluate(() => window.boundary.downloads.files)).toEqual([{ name: 'network.bin', bytes: [0, 255, 7] }])
    await page.evaluate(() => { window.boundary.downloads.mode = 'fail'; window.boundary.loadDownload(`
        const a = document.createElement('a'); a.href = URL.createObjectURL(new Blob(['fail']));
        a.download = 'failed.txt'; a.click();
    `) })
    await expect.poll(() => page.evaluate(() => window.boundary.downloads.errors)).toEqual(['Error: Synthetic storage failure'])
    await page.evaluate(() => { window.boundary.downloads.mode = 'cancel'; window.boundary.loadDownload(`
        const a = document.createElement('a'); a.href = URL.createObjectURL(new Blob(['cancel']));
        a.download = 'cancelled.txt'; a.click();
    `) })
    await expect.poll(() => page.evaluate(() => window.boundary.downloadIdle)).toBe(true)
    expect(await page.evaluate(() => window.boundary.downloads.files.length)).toBe(1)
    expect(await page.evaluate(() => window.boundary.downloads.errors.length)).toBe(1)
})

test('unloading a plugin aborts its pending download without saving or reporting an error', async ({ page, baseURL }) => {
    let release!: () => void
    let started!: () => void
    const ready = new Promise<void>(resolve => { started = resolve })
    const response = new Promise<void>(resolve => { release = resolve })
    await page.route('**/pending-download.bin', async route => {
        started()
        await response
        await route.fulfill({ body: 'too late' }).catch(() => {})
    })
    await page.goto('/')
    await page.evaluate(url => window.boundary.loadDownload(`
        const a = document.createElement('a'); a.href = ${JSON.stringify(url)};
        a.download = 'pending.bin'; a.click();
    `), new URL('/pending-download.bin', baseURL).href)
    await ready
    await page.evaluate(() => window.boundary.unloadDownload())
    release()
    await expect.poll(() => page.evaluate(() => window.boundary.downloadIdle)).toBe(true)
    expect(await page.evaluate(() => window.boundary.downloads.files)).toEqual([])
    expect(await page.evaluate(() => window.boundary.downloads.errors)).toEqual([])
})

test('web mode leaves Blob downloads to the browser', async ({ page }) => {
    await page.goto('/')
    await page.evaluate(() => window.boundary.loadDownload(`
        const a = document.createElement('a');
        a.href = URL.createObjectURL(new Blob(['browser bytes']));
        a.download = 'browser.txt'; a.textContent = 'Download';
        document.body.append(a);
    `, false))
    const downloaded = page.waitForEvent('download')
    await page.frameLocator('[data-risu-plugin-frame]').getByText('Download', { exact: true }).click()
    const file = await downloaded
    expect(file.suggestedFilename()).toBe('browser.txt')
    expect(await readFile((await file.path())!, 'utf8')).toBe('browser bytes')
    expect(await page.evaluate(() => window.boundary.downloads.files)).toEqual([])
})


test('native plugin popups remain sandboxed while normal iframe links remain usable', async ({ page, context }) => {
    await context.route('https://synthetic.invalid/frame', route => route.fulfill({ contentType: 'text/html', headers: { 'Allow-CSP-From': '*' }, body: '<p>Frame destination</p>' }))
    await page.goto('/')
    const original = page.url()
    await page.evaluate(() => window.boundary.loadDownload(`
        const popup = window.open('https://synthetic.invalid/popup');
        document.body.dataset.popup = String(popup === null);
        const a = document.createElement('a');
        a.href = 'https://synthetic.invalid/frame'; a.textContent = 'Frame link'; document.body.append(a);
    `))
    const frame = page.frameLocator('[data-risu-plugin-frame]')
    await expect(frame.locator('body')).toHaveAttribute('data-popup', 'true')
    await frame.getByText('Frame link', { exact: true }).click()
    await expect(frame.getByText('Frame destination')).toBeVisible()
    expect(context.pages()).toHaveLength(1)
    await expect(page).toHaveURL(original)
})

test('main-document downloads cancel default handling before the existing window click listener', async ({ page, context }) => {
    await context.route('https://synthetic.invalid/main.bin', route => route.fulfill({ body: Buffer.from([5, 0, 255]) }))
    await page.goto('/')
    await page.evaluate(() => {
        window.addEventListener('click', event => {
            const anchor = event.target as HTMLAnchorElement
            if (anchor.target === '_blank' && !event.defaultPrevented) document.body.dataset.defaultDownload = 'uncancelled'
        })
        window.boundary.installMainBridge()
        const file = document.createElement('a')
        file.href = 'https://synthetic.invalid/main.bin'
        file.download = 'main.bin'; file.target = '_blank'; document.body.append(file); file.click()
    })
    await expect.poll(() => page.evaluate(() => window.boundary.downloads.files)).toEqual([{ name: 'main.bin', bytes: [5, 0, 255] }])
    expect(await page.locator('body').getAttribute('data-default-download')).toBeNull()
    expect(context.pages()).toHaveLength(1)
})

test('main links use the opener only when replacing the main document', async ({ page }) => {
    await page.goto('/')
    const original = page.url()
    await page.evaluate(() => {
        window.boundary.installMainBridge()
        const a = document.createElement('a')
        a.href = 'https://synthetic.invalid/main'; a.textContent = 'Main link'; document.body.append(a)
        const internal = document.createElement('a')
        internal.href = '#same-document'; document.body.append(internal); internal.click()
        const prevented = document.createElement('a')
        prevented.href = 'https://synthetic.invalid/prevented'; prevented.onclick = event => event.preventDefault()
        document.body.append(prevented); prevented.click()
    })
    await page.getByText('Main link', { exact: true }).click()
    await page.evaluate(() => {
        window.addEventListener('click', event => event.preventDefault())
        window.addEventListener('auxclick', event => event.preventDefault())
        const a = document.createElement('a')
        a.href = 'https://synthetic.invalid/browser-default'; document.body.append(a)
        for (const modifier of ['ctrlKey', 'shiftKey', 'metaKey', 'altKey']) {
            a.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, [modifier]: true }))
        }
        a.dispatchEvent(new MouseEvent('auxclick', { bubbles: true, cancelable: true, button: 1 }))
        a.target = '_blank'; a.click()
    })
    await expect.poll(() => page.evaluate(() => window.boundary.downloads.opened)).toEqual(['https://synthetic.invalid/main'])
    await expect(page).toHaveURL(original + '#same-document')
})

test('main popup windows and named iframe targets keep their native browser behavior', async ({ page, context }) => {
    await context.route('https://synthetic.invalid/popup', route => route.fulfill({ contentType: 'text/html', body: '<script>opener.postMessage("synthetic-auth", "*")</script>' }))
    await context.route('https://synthetic.invalid/embedded', route => route.fulfill({ contentType: 'text/html', body: '<p>Embedded page</p>' }))
    await page.goto('/')
    await page.evaluate(() => {
        window.boundary.installMainBridge()
        window.addEventListener('message', event => { if (event.data === 'synthetic-auth') document.body.dataset.auth = 'received' })
        const frame = document.createElement('iframe'); frame.name = 'embedded'; document.body.append(frame)
        const a = document.createElement('a'); a.href = 'https://synthetic.invalid/embedded'; a.target = 'embedded'; a.textContent = 'Embedded link'; document.body.append(a)
    })
    const popup = page.waitForEvent('popup')
    expect(await page.evaluate(() => !!window.open('https://synthetic.invalid/popup', '_blank'))).toBe(true)
    await expect(page.locator('body')).toHaveAttribute('data-auth', 'received')
    await page.getByText('Embedded link', { exact: true }).click()
    await expect(page.frameLocator('[name="embedded"]').getByText('Embedded page')).toBeVisible()
    expect(await page.evaluate(() => window.boundary.downloads.opened)).toEqual([])
    await (await popup).close()
})
