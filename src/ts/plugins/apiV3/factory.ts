import { recordNativeLogError } from '../../nativeLog'
import { isTauri } from '../../platform'
import { consoleLog, installConsoleCapture, isConsoleLogLevel } from '../../consoleLog'
import { attachPluginReadProvenance, collectPluginReadProvenance, type PluginReadProvenance } from '../pluginReadBaselines'
import { createLocalPluginStorageJsonEncoder, requiresLocalPluginStorageEncoding } from '../localPluginStorageValue'
import { SafeLocalPluginStorage, localPluginStorageJsonWrite } from '../pluginSafeClass'
import { type PluginDownloadHandler } from './pluginDownload'
import { installWebviewBridge } from '../../webviewDownload'

type MsgType =
    | 'CALL_ROOT'
    | 'CALL_INSTANCE'
    | 'INVOKE_CALLBACK'
    | 'CALLBACK_RETURN'
    | 'RESPONSE'
    | 'RELEASE_INSTANCE'
    | 'ABORT_SIGNAL'
    | 'SCRIPT_SETTLED'
    | 'PLUGIN_ERROR'
    | 'PLUGIN_CONSOLE';

interface RpcMessage {
    type: MsgType;
    reqId?: string;
    id?: string;
    method?: string;
    args?: any[];
    result?: any;
    error?: string;
    abortId?: string;
    provenance?: PluginReadProvenance[];
    argProvenance?: PluginReadProvenance[][];
    localPluginJson?: string;
    entries?: { level: unknown; message: unknown; tsMs: unknown }[];
    generation?: number;
}

interface RemoteRef {
    __type: 'REMOTE_REF';
    id: string;
    localPluginStorage?: true;
}

interface CallbackRef {
    __type: 'CALLBACK_REF';
    id: string;
}

interface AbortSignalRef {
    __type: 'ABORT_SIGNAL_REF';
    abortId: string;
    aborted: boolean;
}


const GUEST_BRIDGE_SCRIPT = `
await (async function() {
    const pendingRequests = new Map();
    const callbackRegistry = new Map();
    const callbackIdByFunction = new WeakMap();
    // WeakMap so dropped proxies do not pin their ref ids for the whole
    // plugin lifetime; release() also removes the mapping explicitly.
    const proxyRefRegistry = new WeakMap();
    const abortControllers = new Map();
    const readProvenance = new WeakMap();
    const prepareLocalPluginJson = (${createLocalPluginStorageJsonEncoder.toString()})(${requiresLocalPluginStorageEncoding.toString()});

    function attachReadProvenance(value, entries) {
        for (const entry of entries || []) {
            let object = value;
            for (const key of entry.path) object = object?.[key];
            if (object && typeof object === 'object') readProvenance.set(object, entry.token);
        }
    }

    function collectReadProvenance(value) {
        const entries = [];
        const seen = new WeakSet();
        function visit(object, path) {
            if (!object || typeof object !== 'object' || seen.has(object)) return;
            seen.add(object);
            const token = readProvenance.get(object);
            if (token) entries.push({ path, token });
            if (ArrayBuffer.isView(object)) return;
            for (const [key, child] of Object.entries(object)) visit(child, [...path, Array.isArray(object) ? Number(key) : key]);
        }
        visit(value, []);
        return entries;
    }

    function serializeArg(arg) {
        if (typeof arg === 'function') {
            const existingId = callbackIdByFunction.get(arg);
            if (existingId) {
                return { __type: 'CALLBACK_REF', id: existingId };
            }
            const id = 'cb_' + Math.random().toString(36).substring(2);
            callbackRegistry.set(id, arg);
            callbackIdByFunction.set(arg, id);
            return { __type: 'CALLBACK_REF', id: id };
        }
        if (arg && typeof arg === 'object') {
            const refId = proxyRefRegistry.get(arg);
            if (refId) {
                return { __type: 'REMOTE_REF', id: refId };
            }
            if (arg.constructor === Object) {
                let out = null;
                for (const [key, val] of Object.entries(arg)) {
                    if (val instanceof AbortSignal) {
                        if (!out) out = { ...arg };
                        const abortId = 'abort_' + Math.random().toString(36).substring(2);

                        if (!val.aborted) {
                            val.addEventListener('abort', () => {
                                send({ type: 'ABORT_SIGNAL', abortId });
                            }, { once: true });
                        }

                        out[key] = { __type: 'ABORT_SIGNAL_REF', abortId, aborted: val.aborted };
                    }
                }
                if (out) return out;
            }
        }
        return arg;
    }

    function deserializeResult(val) {
        if (val && typeof val === 'object' && val.__type === 'REMOTE_REF') {
            const proxy = new Proxy({}, {
                get: (target, prop) => {
                    if (prop === 'then') return undefined;
                    if (prop === 'release') {
                        return () => {
                            proxyRefRegistry.delete(proxy);
                            send({ type: 'RELEASE_INSTANCE', id: val.id });
                        };
                    }
                    return (...args) => sendRequest('CALL_INSTANCE', {
                        id: val.id,
                        method: prop,
                        args: args
                    }, val.localPluginStorage === true);
                }
            });
            // Store the mapping so we can serialize it back
            proxyRefRegistry.set(proxy, val.id);
            return proxy;
        }
        if (val && typeof val === 'object' && val.__type === 'CALLBACK_STREAMS') {
            //specialType, one of
            // - Response
            // - none
            const specialType = val.__specialType;
            if (specialType === 'Response') {
                return new Response(val.value, val.init);
            }
            return val.value;
        }
        if (val && typeof val === 'object' && val.__type === 'IFRAME_OBJECT_STREAM') {
            return assembleIframeObjectStream(val.value, val.select);
        }
        return val;
    }

    async function assembleIframeObjectStream(stream, select) {
        const result = {};
        const reader = stream.getReader();
        try {
            while (true) {
                const next = await reader.read();
                if (next.done) {
                    if (select === 'character') return result.characters[0];
                    if (select === 'conversation') return result.characters[0].chats[0];
                    return result;
                }
                const chunks = next.value?.type === 'batch' ? next.value.chunks : [next.value];
                if (!Array.isArray(chunks)) throw new Error('Invalid iframe object stream batch');
                for (const chunk of chunks) {
                    if (chunk?.type === 'provenance') {
                        attachReadProvenance(result, chunk.entries);
                        continue;
                    }
                    if (!chunk || typeof chunk.key !== 'string') {
                        throw new Error('Invalid iframe object stream chunk');
                    }
                    if (chunk.type === 'set') {
                        Object.defineProperty(result, chunk.key, {
                            value: chunk.value, enumerable: true, configurable: true, writable: true
                        });
                    } else if (chunk.type === 'arrayStart') {
                        Object.defineProperty(result, chunk.key, {
                            value: [], enumerable: true, configurable: true, writable: true
                        });
                    } else if (chunk.type === 'arrayPush') {
                        if (!Array.isArray(result[chunk.key])) throw new Error('Invalid iframe array chunk');
                        result[chunk.key].push(chunk.value);
                    } else if (chunk.type === 'characterStart') {
                        result.characters.push({ ...chunk.value, chats: [] });
                    } else if (chunk.type === 'conversationStart') {
                        result.characters.at(-1).chats.push({ ...chunk.value, message: [] });
                    } else if (chunk.type === 'message') {
                        result.characters.at(-1).chats.at(-1).message.push(chunk.value);
                    } else if (chunk.type === 'recordStart') {
                        Object.defineProperty(result, chunk.key, {
                            value: {}, enumerable: true, configurable: true, writable: true
                        });
                    } else if (chunk.type === 'recordSet') {
                        const record = result[chunk.key];
                        if (!record || Object.getPrototypeOf(record) !== Object.prototype || typeof chunk.entryKey !== 'string') {
                            throw new Error('Invalid iframe record chunk');
                        }
                        Object.defineProperty(record, chunk.entryKey, {
                            value: chunk.value, enumerable: true, configurable: true, writable: true
                        });
                    } else {
                        throw new Error('Unknown iframe object stream chunk');
                    }
                }
            }
        } catch (error) {
            await reader.cancel().catch(() => {});
            throw error;
        } finally {
            reader.releaseLock();
        }
    }

    function collectTransferables(obj, transferables = [], seen = new WeakSet()) {
        if (!obj || typeof obj !== 'object' || seen.has(obj)) return transferables;
        seen.add(obj);

        if (obj instanceof ArrayBuffer ||
            obj instanceof MessagePort ||
            obj instanceof ImageBitmap ||
            (typeof OffscreenCanvas !== 'undefined' && obj instanceof OffscreenCanvas)) {
            if (!transferables.includes(obj)) transferables.push(obj);
        }
        else if (ArrayBuffer.isView(obj) && obj.buffer instanceof ArrayBuffer) {
            if (!transferables.includes(obj.buffer)) transferables.push(obj.buffer);
        }
        else if (Array.isArray(obj)) {
            obj.forEach(item => collectTransferables(item, transferables, seen));
        }
        else if (obj.constructor === Object) {
            Object.values(obj).forEach(value => collectTransferables(value, transferables, seen));
        }

        return transferables;
    }

    function replaceStreamsWithPorts(obj) {
        const ports = [];
        const cleanups = [];
        if (!obj || typeof obj !== 'object') return { result: obj, ports, cleanups };

        function replace(val) {
            if (!(val instanceof ReadableStream)) return val;

            const ch = new MessageChannel();
            ports.push(ch.port2);

            const reader = val.getReader();
            let credits = 0;
            let reading = false;
            let finished = false;

            function finish() {
                finished = true;
                ch.port1.onmessage = null;
                ch.port1.close();
            }

            cleanups.push(() => {
                reader.cancel().catch(() => {});
                finish();
            });

            async function pump() {
                if (reading || finished) return;
                reading = true;
                try {
                    while (credits > 0 && !finished) {
                        credits--;
                        const { done, value } = await reader.read();
                        if (finished) return;
                        if (done) { ch.port1.postMessage({ done: true }); finish(); return; }
                        ch.port1.postMessage({ done: false, value });
                    }
                } catch (e) {
                    try { ch.port1.postMessage({ done: true, error: e.message }); } catch(_) {}
                    finish();
                } finally {
                    reading = false;
                }
            }

            ch.port1.onmessage = (e) => {
                if (e.data?.cancel) {
                    reader.cancel();
                    finish();
                } else if (e.data?.pull) {
                    credits++;
                    pump();
                }
            };

            return { __type: 'STREAM_PORT', portIndex: ports.length - 1 };
        }

        if (obj instanceof ReadableStream) return { result: replace(obj), ports, cleanups };
        if (obj.constructor === Object) {
            const out = {};
            for (const k of Object.keys(obj)) out[k] = replace(obj[k]);
            return { result: ports.length ? out : obj, ports, cleanups };
        }

        return { result: obj, ports, cleanups };
    }

    function reconstructStreamsFromPorts(obj, ports) {
        if (!obj || typeof obj !== 'object') return obj;

        function reconstruct(val) {
            if (!val || val.__type !== 'STREAM_PORT' || typeof val.portIndex !== 'number') return val;

            const port = ports[val.portIndex];
            if (!port) throw new Error('Stream port at index ' + val.portIndex + ' not received');

            return new ReadableStream({
                start(controller) {
                    port.onmessage = (e) => {
                        if (e.data.done) {
                            if (e.data.error) controller.error(new Error(e.data.error));
                            else controller.close();
                            port.onmessage = null;
                            port.close();
                        } else {
                            controller.enqueue(e.data.value);
                        }
                    };
                },
                pull() {
                    port.postMessage({ pull: true });
                },
                cancel() {
                    port.postMessage({ cancel: true });
                    port.onmessage = null;
                    port.close();
                }
            }, { highWaterMark: 0 });
        }

        if (obj.__type === 'STREAM_PORT') return reconstruct(obj);
        if (Object.prototype.toString.call(obj) === '[object Object]') {
            const out = {};
            let changed = false;
            for (const k of Object.keys(obj)) {
                out[k] = reconstruct(obj[k]);
                if (out[k] !== obj[k]) changed = true;
            }
            return changed ? out : obj;
        }

        return obj;
    }

    function send(payload, transferables = []) {
        window.parent.postMessage(payload, '*', transferables);
    }

    const nonTextInputTypes = new Set([
        'button', 'checkbox', 'color', 'file', 'hidden', 'image',
        'radio', 'range', 'reset', 'submit'
    ]);
    let focusedInputReportFrame = 0;

    function getFocusedTextInput() {
        const element = document.activeElement;
        if (element instanceof HTMLTextAreaElement) {
            return !element.disabled && !element.readOnly ? element : null;
        }
        if (element instanceof HTMLInputElement) {
            return !element.disabled && !element.readOnly && !nonTextInputTypes.has(element.type)
                ? element
                : null;
        }
        return element instanceof HTMLElement && element.isContentEditable ? element : null;
    }

    function reportFocusedInput() {
        focusedInputReportFrame = 0;
        const element = getFocusedTextInput();
        if (!element) {
            send({ type: 'RISU_PLUGIN_INPUT_FOCUS', rect: null });
            return;
        }

        const rect = element.getBoundingClientRect();
        send({
            type: 'RISU_PLUGIN_INPUT_FOCUS',
            rect: { top: rect.top, bottom: rect.bottom }
        });
    }

    function scheduleFocusedInputReport() {
        if (!focusedInputReportFrame) {
            focusedInputReportFrame = requestAnimationFrame(reportFocusedInput);
        }
    }

    document.addEventListener('focusin', reportFocusedInput);
    document.addEventListener('focusout', scheduleFocusedInputReport);
    window.addEventListener('scroll', scheduleFocusedInputReport, true);

    function sendRequest(type, payload, localPluginStorage = false) {
        return new Promise((resolve, reject) => {
            if (localPluginStorage && payload.method === 'setItem' && payload.args?.length === 2 && typeof payload.args[0] === 'string') {
                const serialized = prepareLocalPluginJson(payload.args[1]);
                if (serialized !== undefined) {
                    payload.localPluginJson = serialized;
                    payload.args = [payload.args[0]];
                }
            }
            const reqId = Math.random().toString(36).substring(2);
            pendingRequests.set(reqId, { resolve, reject });


            if (payload.args) {
                payload.argProvenance = payload.args.map(collectReadProvenance);
                payload.args = payload.args.map(serializeArg);
            }

            const message = { type: type, reqId: reqId, ...payload };
            const transferables = collectTransferables(message);
            send(message, transferables);
        });
    }

    
    
    
    window.addEventListener('message', async (event) => {
        if (event.source !== window.parent) return;
        const data = event.data;
        if (!data || typeof data !== 'object' || Array.isArray(data)) return;
        const hasId = value => typeof value === 'string' && value.length > 0;
        switch (data.type) {
            case 'RESPONSE':
                if (!hasId(data.reqId) || (data.error !== undefined && typeof data.error !== 'string')) return;
                break;
            case 'EXECUTE_CODE':
                if (!hasId(data.reqId) || typeof data.code !== 'string') return;
                break;
            case 'ABORT_SIGNAL':
                if (!hasId(data.abortId)) return;
                break;
            case 'INVOKE_CALLBACK':
                if (!hasId(data.reqId) || !hasId(data.id) || !Array.isArray(data.args)) return;
                if (data.args.some(arg => arg && arg.__type === 'ABORT_SIGNAL_REF'
                    && (!hasId(arg.abortId) || typeof arg.aborted !== 'boolean'))) return;
                break;
            default:
                return;
        }


        if (data.type === 'RESPONSE' && data.reqId) {
            const req = pendingRequests.get(data.reqId);
            if (req) {
                if (data.error) req.reject(new Error(data.error));
                else {
                    try {
                        const result = await deserializeResult(reconstructStreamsFromPorts(data.result, event.ports));
                        attachReadProvenance(result, data.provenance);
                        req.resolve(result);
                    } catch (e) {
                        req.reject(e);
                    }
                }
                pendingRequests.delete(data.reqId);
            }
        }

        else if (data.type === 'EXECUTE_CODE' && data.reqId) {
            const response = { type: 'EXEC_RESULT', reqId: data.reqId };
            try {
                const result = await eval('(async () => {' + data.code + '})()');
                response.result = result;
            } catch (e) {
                response.error = e.message || String(e);
            }
            send(response);
        }

        else if (data.type === 'ABORT_SIGNAL' && data.abortId) {
            const controller = abortControllers.get(data.abortId);
            if (controller) {
                controller.abort();
                abortControllers.delete(data.abortId);
            }
        }

        else if (data.type === 'INVOKE_CALLBACK' && data.id) {
            const fn = callbackRegistry.get(data.id);
            const response = { type: 'CALLBACK_RETURN', reqId: data.reqId };
            const usedAbortIds = [];
            let transferables = [];
            let streamCleanups = [];

            const rollbackStreams = () => {
                for (const cleanup of streamCleanups) {
                    try { cleanup(); } catch(_) {}
                }
                streamCleanups = [];
            };

            try {
                if (!fn) throw new Error("Callback not found or released");
                const deserializedArgs = (data.args || []).map(function(a) {
                    if (a && typeof a === 'object' && a.__type === 'ABORT_SIGNAL_REF') {
                        const controller = new AbortController();
                        abortControllers.set(a.abortId, controller);
                        usedAbortIds.push(a.abortId);
                        if (a.aborted) { controller.abort(); }
                        return controller.signal;
                    }
                    return deserializeResult(a);
                });
                const result = await fn(...deserializedArgs);
                response.result = result;
                const { result: streamResult, ports: streamPorts, cleanups } = replaceStreamsWithPorts(response.result);
                response.result = streamResult;
                streamCleanups = cleanups;
                transferables = collectTransferables(response, streamPorts);
            } catch (e) {
                rollbackStreams();
                delete response.result;
                response.error = (e && e.message) || String(e || "Guest callback error");
            }
            // Clean up abort controllers after callback completes
            for (const id of usedAbortIds) {
                abortControllers.delete(id);
            }
            try {
                send(response, transferables);
            } catch (e) {
                rollbackStreams();
                try {
                    send({
                        type: 'CALLBACK_RETURN',
                        reqId: data.reqId,
                        error: 'Failed to post message to parent: ' + ((e && e.message) || String(e || "Unknown error"))
                    });
                } catch(_) {}
            }
        }
    });





    const propertyCache = new Map();

    window.risuai = new Proxy({}, {
        get: (target, prop) => {
            if (propertyCache.has(prop)) {
                return propertyCache.get(prop);
            }
            return (...args) => sendRequest('CALL_ROOT', { method: prop, args: args });
        }
    });
    window.Risuai = window.risuai;

    try {
        // Initialize cached properties
        const propsToInit = await window.risuai._getPropertiesForInitialization();
        for (let i = 0; i < propsToInit.list.length; i++) {
            const key = propsToInit.list[i];
            const value = propsToInit[key];
            propertyCache.set(key, value);
        }

        // Initialize aliases
        const aliases = await window.risuai._getAliases();
        const aliasKeys = Object.keys(aliases);
        for (let i = 0; i < aliasKeys.length; i++) {
            const aliasKey = aliasKeys[i];
            const childrens = Object.keys(aliases[aliasKey]);
            const aliasObj = {};
            for (let j = 0; j < childrens.length; j++) {
                const childKey = childrens[j];
                aliasObj[childKey] = risuai[aliases[aliasKey][childKey]];
            }
            propertyCache.set(aliasKey, aliasObj);
        }

        // Initialize helper functions defined in the guest

        propertyCache.set('unwarpSafeArray', async (safeArray) => {
            const length = await safeArray.length();
            const result = [];
            for (let i = 0; i < length; i++) {
                const item = await safeArray.at(i);
                result.push(item);
            }
            return result;
        });
    } catch (e) {
        console.error('Failed to initialize risuai properties:', e);
    }

    window.initOldApiGlobal = () => {
        const keys = risuai._getOldKeys()
        for(const key of keys){
            window[key] = risuai[key];
        }
    }

    Object.freeze(window.postMessage);
})();
`;

// risuFetch takes GlobalFetchArgs, which names its signal abortSignal.
const requestSignalKeys = new Map([['nativeFetch', 'signal'], ['risuFetch', 'abortSignal']]);
export const UNREAD_PLUGIN_STREAM_TIMEOUT_MS = 60_000;

export class SandboxHost {
    private iframe: HTMLIFrameElement;
    private apiFactory: any;
    private terminated = false;
    private nonce = crypto.randomUUID();
    private csp = `connect-src 'none'; script-src 'nonce-${this.nonce}' 'wasm-unsafe-eval'; frame-src 'none'; object-src 'none'; style-src * 'unsafe-inline'; default-src 'none'; img-src * data: blob:; font-src * data: blob:; media-src * data: blob:; base-uri 'none';`;

    private instanceRegistry = new Map<string, any>();
    private abortControllers = new Map<string, AbortController>();
    private messageHandlerRef: ((event: MessageEvent) => void) | null = null;
    private disposeConsoleClear: (() => void) | null = null;
    private callbackWrapperCache = new Map<string, Function>();

    private pendingCallbacks = new Map<string, { resolve: Function, reject: Function, dispose: () => void }>();
    private pendingStorageMutations = 0;
    private storageDrainWaiters = new Set<() => void>();

    public drainStorageMutations(): Promise<void> {
        if (this.pendingStorageMutations === 0) return Promise.resolve();
        return new Promise(resolve => this.storageDrainWaiters.add(resolve));
    }

    // The guest reports when its top level script has settled. Until the calls
    // it made before that have answered, the script is not finished.
    private scriptSettledListener: (() => void) | null = null;
    private scriptSettledReported = false;
    private guestReportedSettled = false;
    private pendingHostCalls = 0;
    private unloading = false;
    private invokingUnload = false;
    private activityListener: (() => void) | null = null;

    public onActivityChanged(listener: () => void) {
        this.activityListener = listener;
    }

    public isIdle(): boolean {
        return this.terminated || (this.guestReportedSettled && this.pendingHostCalls === 0
            && this.pendingCallbacks.size === 0 && this.activeStreamCleanups.size === this.unreadStreamCleanups.size);
    }

    public beginUnload(): void {
        this.unloading = true;
    }

    public invokeUnloadCallback(callback: Function): unknown {
        this.invokingUnload = true;
        try { return callback(); }
        finally { this.invokingUnload = false; }
    }

    // Teardown hooks for streams bridged over MessagePort. MessagePort has no
    // 'close' event in stable browsers, so without these the other side of an
    // active stream would wait forever once the iframe is gone.
    private activeStreamCleanups = new Set<() => void>();
    private unreadStreamCleanups = new Set<() => void>();

    constructor(apiFactory: any, private download?: PluginDownloadHandler) {
        this.apiFactory = apiFactory;
    }

    /** Called once, after the guest script settles and its calls have answered. */
    public onScriptSettled(listener: () => void) {
        this.scriptSettledListener = listener;
    }

    private reportScriptSettled() {
        if (this.scriptSettledReported) return;
        if (!this.guestReportedSettled || this.pendingHostCalls > 0) return;
        this.scriptSettledReported = true;
        const listener = this.scriptSettledListener;
        this.scriptSettledListener = null;
        listener?.();
    }

    public executeInIframe(code: string): Promise<any> {
        return new Promise((resolve, reject) => {
            const reqId = 'exec_' + Math.random().toString(36).substring(2);

            const handler = (event: MessageEvent) => {
                if (event.source !== this.iframe.contentWindow) return;
                const data = event.data;

                if (data && typeof data === 'object' && data.type === 'EXEC_RESULT' && data.reqId === reqId) {
                    window.removeEventListener('message', handler);
                    if (data.error) {
                        reject(new Error(data.error));
                    } else {
                        resolve(data.result);
                    }
                }
            };

            window.addEventListener('message', handler);

            this.iframe.contentWindow?.postMessage({
                type: 'EXECUTE_CODE',
                reqId,
                code
            }, '*');
        });
    }

    private collectTransferables(obj: any, transferables: Transferable[] = [], seen = new WeakSet<object>()): Transferable[] {
        if (!obj || typeof obj !== 'object' || seen.has(obj)) return transferables;
        seen.add(obj);

        if (obj instanceof ArrayBuffer ||
            obj instanceof MessagePort ||
            obj instanceof ImageBitmap ||
            obj instanceof WritableStream ||
            obj instanceof TransformStream ||
            (typeof OffscreenCanvas !== 'undefined' && obj instanceof OffscreenCanvas)) {
            if (!transferables.includes(obj)) transferables.push(obj);
        }
        else if (ArrayBuffer.isView(obj) && obj.buffer instanceof ArrayBuffer) {
            if (!transferables.includes(obj.buffer)) transferables.push(obj.buffer);
        }
        else if (Array.isArray(obj)) {
            obj.forEach(item => this.collectTransferables(item, transferables, seen));
        }
        else if (obj.constructor === Object) {
            Object.values(obj).forEach(value => this.collectTransferables(value, transferables, seen));
        }

        return transferables;
    }


    private serialize(val: any): any {
        if (
            val &&
            (typeof val === 'object' || typeof val === 'function') &&
            val.__classType === 'REMOTE_REQUIRED'
        ) {
            if (val === null) return null;
            if (Array.isArray(val)) return val;


            const id = 'ref_' + Math.random().toString(36).substring(2);
            this.instanceRegistry.set(id, val);
            return { __type: 'REMOTE_REF', id,
                ...(val instanceof SafeLocalPluginStorage ? { localPluginStorage: true } : {}) } as RemoteRef;
        }

        if(val instanceof Response) {
            return {
                __type: 'CALLBACK_STREAMS',
                __specialType: 'Response',
                value: val.body,
                init: {
                    status: val.status,
                    statusText: val.statusText,
                    headers: Array.from(val.headers.entries())
                }
            };
        }

        if(
            val instanceof WritableStream
            || val instanceof TransformStream
        ) {
            return {
                __type: 'CALLBACK_STREAMS',
                __specialType: 'none',
                value: val
            };
        }
        return val;
    }


    private deserializeArgs(args: any[], usedAbortIds?: string[]) {
        return args.map(arg => {
            if (arg && arg.__type === 'CALLBACK_REF') {
                const cbRef = arg as CallbackRef;

                const cached = this.callbackWrapperCache.get(cbRef.id);
                if (cached) return cached;

                const wrapper = (...innerArgs: any[]) => {
                    // After terminate() the iframe is gone; posting would be a
                    // no-op and the promise would never settle, hanging callers
                    // (e.g. chat output dispatch) that await this callback.
                    if (this.terminated || (this.unloading && !this.invokingUnload) || !this.iframe?.contentWindow) {
                        const rejected = Promise.reject(new Error('Plugin sandbox terminated'));
                        // DOM notifications have no caller awaiting their callback.
                        void rejected.catch(() => {});
                        return rejected;
                    }
                    return new Promise((resolve, reject) => {
                        const reqId = 'cb_req_' + Math.random().toString(36).substring(2);
                        const abortListeners: Array<() => void> = [];
                        const dispose = () => abortListeners.forEach(remove => remove());
                        this.pendingCallbacks.set(reqId, { resolve, reject, dispose });
                        this.activityListener?.();
                        try {
                            // AbortSignal cannot be structured-cloned for postMessage.
                            // Convert to a serializable ref and forward abort events
                            // via a separate ABORT_SIGNAL message.
                            const sanitizedArgs = innerArgs.map(arg => {
                                if (arg instanceof AbortSignal) {
                                    const abortId = 'abort_' + Math.random().toString(36).substring(2);
                                    const ref: AbortSignalRef = {
                                        __type: 'ABORT_SIGNAL_REF',
                                        abortId,
                                        aborted: arg.aborted
                                    };
                                    if (!arg.aborted) {
                                        const forwardAbort = () => {
                                            try {
                                                this.iframe.contentWindow?.postMessage({
                                                    type: 'ABORT_SIGNAL',
                                                    abortId
                                                } as RpcMessage, '*');
                                            } catch (_) { /* iframe already removed */ }
                                        };
                                        arg.addEventListener('abort', forwardAbort, { once: true });
                                        abortListeners.push(() => arg.removeEventListener('abort', forwardAbort));
                                    }
                                    return ref;
                                }
                                return this.serialize(arg);
                            });

                            const message = {
                                type: 'INVOKE_CALLBACK',
                                id: cbRef.id,
                                reqId,
                                args: sanitizedArgs
                            };
                            const transferables = this.collectTransferables(message);
                            this.iframe.contentWindow?.postMessage(message, '*', transferables);
                        } catch (error) {
                            dispose();
                            this.pendingCallbacks.delete(reqId);
                            this.activityListener?.();
                            reject(error);
                        }
                    });
                };
                this.callbackWrapperCache.set(cbRef.id, wrapper);
                return wrapper;
            }
            if (arg && arg.__type === 'REMOTE_REF') {
                const remoteRef = arg as RemoteRef;
                const instance = this.instanceRegistry.get(remoteRef.id);
                if (instance) {
                    return instance;
                }
                throw new Error(`Remote instance ${remoteRef.id} not found or already released`);
            }
            if (arg && typeof arg === 'object' && arg.constructor === Object) {
                let out: any = null;
                for (const [key, val] of Object.entries<any>(arg)) {
                    if (val && val.__type === 'ABORT_SIGNAL_REF') {
                        if (!out) out = { ...arg };
                        const abortRef = val as AbortSignalRef, controller = new AbortController();

                        if (abortRef.aborted) controller.abort();
                        else this.abortControllers.set(abortRef.abortId, controller);

                        usedAbortIds?.push(abortRef.abortId);
                        out[key] = controller.signal;
                    }
                }
                if (out) return out;
            }
            return arg;
        });
    }

    private replaceStreamsWithPorts(obj: any): { result: any, ports: MessagePort[], cleanups: (() => void)[] } {
        const ports: MessagePort[] = [];
        const cleanups: (() => void)[] = [];
        if (!obj || typeof obj !== 'object') return { result: obj, ports, cleanups };

        const replace = (val: any): any => {
            if (!(val instanceof ReadableStream)) return val;

            const ch = new MessageChannel();
            ports.push(ch.port2);

            const reader = val.getReader();
            let credits = 0;
            let reading = false;
            let finished = false;
            let unreadTimeout: ReturnType<typeof setTimeout>;

            const finish = () => {
                finished = true;
                clearTimeout(unreadTimeout);
                ch.port1.onmessage = null;
                ch.port1.close();
                this.activeStreamCleanups.delete(cleanup);
                this.unreadStreamCleanups.delete(cleanup);
                this.activityListener?.();
            };

            const cleanup = () => {
                if (finished) return;
                reader.cancel().catch(() => {});
                finish();
            };
            this.activeStreamCleanups.add(cleanup);
            this.unreadStreamCleanups.add(cleanup);
            unreadTimeout = setTimeout(() => {
                try { ch.port1.postMessage({ done: true, error: 'Unread plugin response expired' }); } catch (_) {}
                cleanup();
            }, UNREAD_PLUGIN_STREAM_TIMEOUT_MS);
            this.activityListener?.();
            cleanups.push(cleanup);

            const pump = async () => {
                if (reading || finished) return;
                reading = true;
                try {
                    while (credits > 0 && !finished) {
                        credits--;
                        const { done, value } = await reader.read();
                        if (finished) return;
                        if (done) { ch.port1.postMessage({ done: true }); finish(); return; }
                        ch.port1.postMessage({ done: false, value });
                    }
                } catch (e: any) {
                    try { ch.port1.postMessage({ done: true, error: e.message }); } catch(_) {}
                    finish();
                } finally {
                    reading = false;
                }
            };

            ch.port1.onmessage = (e: MessageEvent) => {
                if (e.data?.cancel) {
                    reader.cancel();
                    finish();
                } else if (e.data?.pull) {
                    clearTimeout(unreadTimeout);
                    if (this.unreadStreamCleanups.delete(cleanup)) this.activityListener?.();
                    credits++;
                    pump();
                }
            };

            return { __type: 'STREAM_PORT', portIndex: ports.length - 1 };
        };

        if (obj instanceof ReadableStream) return { result: replace(obj), ports, cleanups };
        if (obj.constructor === Object) {
            const out: any = {};
            for (const k of Object.keys(obj)) out[k] = replace(obj[k]);
            return { result: ports.length ? out : obj, ports, cleanups };
        }

        return { result: obj, ports, cleanups };
    }

    private reconstructStreamsFromPorts(obj: any, ports: readonly MessagePort[]): any {
        if (!obj || typeof obj !== 'object') return obj;

        const reconstruct = (val: any): any => {
            if (val?.__type !== 'STREAM_PORT' || typeof val.portIndex !== 'number') return val;

            const port = ports[val.portIndex];
            if (!port) throw new Error(`Stream port at index ${val.portIndex} not received`);

            const cleanups = this.activeStreamCleanups;
            let cleanup: (() => void) | null = null;
            const unregister = () => {
                if (cleanup) {
                    cleanups.delete(cleanup);
                    cleanup = null;
                    this.activityListener?.();
                }
            };

            const activityChanged = () => this.activityListener?.();
            return new ReadableStream({
                start(controller) {
                    port.onmessage = (e: MessageEvent) => {
                        if (e.data.done) {
                            if (e.data.error) controller.error(new Error(e.data.error));
                            else controller.close();
                            port.onmessage = null;
                            port.close();
                            unregister();
                        } else {
                            controller.enqueue(e.data.value);
                        }
                    };
                    cleanup = () => {
                        controller.error(new Error('Sandbox terminated'));
                        port.onmessage = null;
                        port.close();
                    };
                    cleanups.add(cleanup);
                    activityChanged();
                },
                pull() {
                    port.postMessage({ pull: true });
                },
                cancel() {
                    port.postMessage({ cancel: true });
                    port.onmessage = null;
                    port.close();
                    unregister();
                }
            });
        };

        if (obj.__type === 'STREAM_PORT') return reconstruct(obj);
        if (obj.constructor === Object) {
            const out: any = {};
            let changed = false;
            for (const k of Object.keys(obj)) {
                out[k] = reconstruct(obj[k]);
                if (out[k] !== obj[k]) changed = true;
            }
            return changed ? out : obj;
        }

        return obj;
    }

    private disposeUndeliveredResult(result: any) {
        const streams = new Set<any>();
        const collect = (value: any) => {
            if (value instanceof Response) value = value.body;
            if (value instanceof ReadableStream || value instanceof WritableStream || value instanceof TransformStream) {
                streams.add(value);
            }
        };
        collect(result);
        if (result && result.constructor === Object) Object.values(result).forEach(collect);
        for (const stream of streams) {
            if (stream instanceof ReadableStream) void stream.cancel().catch(() => {});
            else if (stream instanceof WritableStream) void stream.abort().catch(() => {});
            else {
                void stream.readable.cancel().catch(() => {});
                void stream.writable.abort().catch(() => {});
            }
        }
    }

    private closeActiveStreams() {
        for (const cleanup of [...this.activeStreamCleanups]) {
            try { cleanup(); } catch(_) {}
        }
        this.activeStreamCleanups.clear();
    }

    public run(container: HTMLElement|HTMLIFrameElement, userCode: string, sourceLabel = 'anonymous') {
        if (this.terminated) throw new Error('Plugin sandbox terminated');
        if(container instanceof HTMLIFrameElement) {
            this.iframe = container;
        } else {
            this.iframe = document.createElement('iframe');
            container.appendChild(this.iframe);
        }

        this.iframe.style.width = "100%";
        this.iframe.style.height = "100%";
        this.iframe.style.border = "none";

        this.iframe.style.backgroundColor = "transparent";
        // A frame whose color scheme differs from its document's is painted opaque, so keep the
        // plugin document's default scheme instead of inheriting the app theme's.
        this.iframe.style.colorScheme = "normal";
        this.iframe.setAttribute('allowTransparency', 'true');
        this.iframe.setAttribute('data-risu-plugin-frame', '');

        this.iframe.sandbox.add('allow-scripts');
        this.iframe.sandbox.add('allow-modals')
        this.iframe.sandbox.add('allow-downloads')

        this.iframe.setAttribute('csp', this.csp);

        const messageHandler = async (event: MessageEvent) => {
            if (this.terminated || event.source !== this.iframe.contentWindow) return;
            const data = event.data as RpcMessage;
            if (!data || typeof data !== 'object' || Array.isArray(data)) return;

            if (data.type === 'PLUGIN_CONSOLE') {
                if (!Array.isArray(data.entries)) return;
                if (data.generation !== consoleLog.generation) {
                    // A frame can finish loading after its clear notification was sent.
                    this.iframe.contentWindow?.postMessage({ type: 'CLEAR_CONSOLE', generation: consoleLog.generation }, '*');
                    return;
                }
                for (const entry of data.entries.slice(0, 200)) {
                    if (!entry || !isConsoleLogLevel(entry.level) || typeof entry.message !== 'string') continue;
                    consoleLog.append(entry.level, sourceLabel, entry.message,
                        typeof entry.tsMs === 'number' && Number.isFinite(entry.tsMs) && Math.abs(entry.tsMs) < 8.64e15
                            ? entry.tsMs : Date.now());
                }
                return;
            }

            if (data.type === 'PLUGIN_ERROR') {
                if (typeof data.error === 'string' && data.error.trim()) {
                    const message = `[${sourceLabel}] ${data.error.slice(0, 8192)}`;
                    if (isTauri) void recordNativeLogError(message).catch(() => {});
                }
                return;
            }


            if (data.type === 'CALLBACK_RETURN') {
                const req = this.pendingCallbacks.get(data.reqId!);
                if (req) {
                    req.dispose();
                    if (data.error) req.reject(new Error(data.error));
                    else {
                        try {
                            req.resolve(this.reconstructStreamsFromPorts(data.result, event.ports));
                        } catch (e) {
                            req.reject(e);
                        }
                    }
                    this.pendingCallbacks.delete(data.reqId!);
                    this.activityListener?.();
                }
                return;
            }

            if (data.type === 'ABORT_SIGNAL') {
                const controller = this.abortControllers.get(data.abortId!);
                if (controller) {
                    controller.abort();
                    this.abortControllers.delete(data.abortId!);
                }
                return;
            }


            if (data.type === 'RELEASE_INSTANCE') {
                this.instanceRegistry.delete(data.id!);
                return;
            }

            if (data.type === 'SCRIPT_SETTLED') {
                this.guestReportedSettled = true;
                this.reportScriptSettled();
                this.activityListener?.();
                return;
            }


            if (data.type === 'CALL_ROOT' || data.type === 'CALL_INSTANCE') {
                const response: RpcMessage = { type: 'RESPONSE', reqId: data.reqId };
                const usedAbortIds: string[] = [];
                let transferables: Transferable[] = [];
                let streamCleanups: (() => void)[] = [];

                const rollbackStreams = () => {
                    for (const cleanup of streamCleanups) {
                        try { cleanup(); } catch(_) {}
                    }
                    streamCleanups = [];
                };

                this.pendingHostCalls += 1;
                this.activityListener?.();
                const storageMutation = data.type === 'CALL_INSTANCE'
                    ? ['setItem', 'removeItem', 'clear'].includes(String(data.method))
                    : ['setChar', 'setCharacter', 'setCharacterToIndex', 'setChatToIndex', 'risunestPatchConversation', 'setDatabase', 'setDatabaseLite', '_setPluginStorage', '_removePluginStorage', '_clearPluginStorage', '_setSafeLocalStorage', '_removeSafeLocalStorage', '_clearSafeLocalStorage'].includes(String(data.method));
                if (storageMutation) this.pendingStorageMutations += 1;
                try {

                    if (Object.hasOwn(data, 'localPluginJson') && (data.type !== 'CALL_INSTANCE' || data.method !== 'setItem')) {
                        throw new TypeError('Invalid local plugin JSON write');
                    }

                    const args = this.deserializeArgs(data.args || [], usedAbortIds);
                    const signalKey = data.type === 'CALL_ROOT' ? requestSignalKeys.get(String(data.method)) : undefined;
                    let downloadSignal: AbortSignal | undefined;
                    if (data.type === 'CALL_ROOT' && data.method === '_downloadFile' && this.download) {
                        const id = crypto.randomUUID();
                        const controller = new AbortController();
                        this.abortControllers.set(id, controller);
                        usedAbortIds.push(id);
                        downloadSignal = controller.signal;
                    }
                    if (signalKey && !args[1]?.[signalKey]) {
                        const id = crypto.randomUUID();
                        const controller = new AbortController();
                        this.abortControllers.set(id, controller);
                        usedAbortIds.push(id);
                        args[1] = { ...args[1], [signalKey]: controller.signal };
                    }
                    if (Array.isArray(data.argProvenance)) {
                        args.forEach((arg, index) => attachPluginReadProvenance(arg, data.argProvenance![index] ?? []));
                    }
                    let result: any;


                    if (data.type === 'CALL_ROOT') {
                        if (downloadSignal) {
                            result = await this.download!(args[0], args[1], downloadSignal);
                        } else {
                            const fn = this.apiFactory[data.method!];
                            if (typeof fn !== 'function') throw new Error(`API method ${data.method} not found`);
                            result = await fn(...args);
                        }
                    } else {
                        const instance = this.instanceRegistry.get(data.id!);
                        if (!instance) throw new Error("Instance not found or released");
                        if (typeof instance[data.method!] !== 'function') throw new Error(`Method ${data.method} missing on instance`);
                        if (Object.hasOwn(data, 'localPluginJson')) {
                            if (!(instance instanceof SafeLocalPluginStorage) || args.length !== 1) {
                                throw new TypeError('Invalid local plugin JSON write');
                            }
                            result = await instance[localPluginStorageJsonWrite](args[0], data.localPluginJson!);
                        } else {
                            result = await instance[data.method!](...args);
                        }
                    }


                    if (this.terminated || !this.iframe.contentWindow) {
                        this.disposeUndeliveredResult(result);
                        return;
                    }
                    response.result = this.serialize(result);
                    response.provenance = collectPluginReadProvenance(result);
                    const { result: streamResult, ports: streamPorts, cleanups } = this.replaceStreamsWithPorts(response.result);
                    response.result = streamResult;
                    streamCleanups = cleanups;
                    transferables = this.collectTransferables(response, streamPorts);

                } catch (err: any) {
                    rollbackStreams();
                    delete response.result;
                    const method = typeof data.method === 'string' && /^[A-Za-z_][A-Za-z0-9_]{0,79}$/.test(data.method)
                        ? data.method : 'unknown';
                    response.error = `[Plugin API: ${method}] ` + (err?.message || String(err || "Host execution error"));
                } finally {
                    for (const id of usedAbortIds) this.abortControllers.delete(id);
                    this.pendingHostCalls -= 1;
                    if (storageMutation && --this.pendingStorageMutations === 0) {
                        for (const resolve of this.storageDrainWaiters) resolve();
                        this.storageDrainWaiters.clear();
                    }
                    this.reportScriptSettled();
                    this.activityListener?.();
                }

                if (import.meta.env.DEV) {
                    console.log("Original request:", data);
                    console.log('Original response:', response, transferables);
                }
                const target = this.terminated ? null : this.iframe.contentWindow;
                if (!target) {
                    rollbackStreams();
                    return;
                }
                try {
                    target.postMessage(response, '*', transferables);
                } catch (error) {
                    rollbackStreams();
                    try {
                        this.iframe.contentWindow?.postMessage({
                            type: 'RESPONSE',
                            reqId: data.reqId,
                            error: 'Failed to post message to iframe: ' + ((error as any)?.message || String(error || "Unknown error"))
                        }, '*');
                    } catch(_) {}
                    console.error('Failed to post message to iframe:', error);
                }
            }
        };

        this.messageHandlerRef = messageHandler;
        window.addEventListener('message', this.messageHandlerRef);
        this.disposeConsoleClear = consoleLog.onClear(generation => {
            this.iframe.contentWindow?.postMessage({ type: 'CLEAR_CONSOLE', generation }, '*');
        });


        const html = `
      <!DOCTYPE html>
      <html>
      <head>
        <meta charset="UTF-8">
        <meta http-equiv="Content-Security-Policy" content="${this.csp}" id="csp-meta">
      </head>
      <body>
        <style>
            body {
                background-color: transparent;
            }
        </style>
        <script nonce="${this.nonce}">
            document.querySelector('meta#csp-meta')?.remove();
            let consoleEntries = [];
            let consoleChars = 0;
            let consoleDropped = 0;
            let consoleTimer;
            let consoleGeneration = ${consoleLog.generation};
            window.addEventListener('message', event => {
                if (event.source !== parent || event.data?.type !== 'CLEAR_CONSOLE') return;
                consoleGeneration = event.data.generation;
                consoleEntries = [];
                consoleChars = consoleDropped = 0;
                clearTimeout(consoleTimer);
                consoleTimer = undefined;
            });
            const flushConsole = () => {
                clearTimeout(consoleTimer);
                consoleTimer = undefined;
                if (consoleDropped) consoleEntries.push({ level: 'warn', message: consoleDropped + ' console messages omitted', tsMs: consoleEntries[consoleEntries.length - 1].tsMs });
                const entries = consoleEntries;
                consoleEntries = [];
                consoleChars = consoleDropped = 0;
                if (entries.length) {
                    try { parent.postMessage({ type: 'PLUGIN_CONSOLE', entries, generation: consoleGeneration }, '*'); } catch (_) {}
                }
            };
            (${installConsoleCapture.toString()})(console, (level, message) => {
                while (consoleEntries.length && (consoleEntries.length >= 199 || consoleChars + message.length > 262144)) {
                    consoleChars -= consoleEntries.shift().message.length;
                    consoleDropped++;
                }
                consoleEntries.push({ level, message, tsMs: Date.now() });
                consoleChars += message.length;
                consoleTimer ??= setTimeout(flushConsole, 100);
            });
            const reportedErrors = new WeakSet();
            const reportPluginError = (error) => {
                if (error && typeof error === 'object') {
                    if (reportedErrors.has(error)) return;
                    reportedErrors.add(error);
                }
                const parts = [];
                const seen = new Set();
                let current = error;
                while (current != null && parts.length < 5 && !seen.has(current)) {
                    seen.add(current);
                    try {
                        parts.push(current.message ? (current.name ? current.name + ': ' : '') + current.message : String(current));
                    } catch (_) { parts.push('Unprintable plugin error'); }
                    try { current = current.cause; } catch (_) { break; }
                }
                const message = parts.join('\\n').slice(0, 8192);
                try { console.error(message); } catch (_) {}
                flushConsole();
                try { parent.postMessage({ type: 'PLUGIN_ERROR', error: message }, '*'); } catch (_) {}
            };
            window.addEventListener('error', event => { if (event.error) reportPluginError(event.error); });
            window.addEventListener('unhandledrejection', event => reportPluginError(event.reason));
            (async () => {
                ${GUEST_BRIDGE_SCRIPT}
                ${this.download ? `(${installWebviewBridge.toString()})((name, source) => window.risuai._downloadFile(name, source), reportPluginError);` : ''}
                    
                const settled = () => {
                    flushConsole();
                    try { parent.postMessage({ type: 'SCRIPT_SETTLED' }, '*'); } catch (_) {}
                };
                (async () => {
                    ${userCode}
                })().then(settled, error => { try { reportPluginError(error); } finally { settled(); } })
            })();
            //# sourceURL=risu-plugin-v3/${encodeURIComponent(sourceLabel)}.js
        </script>
      </body>
      </html>
    `;

        this.iframe.srcdoc = html;

        return () => {
            this.terminate();
        };
    }

    public terminate() {
        this.terminated = true;
        this.disposeConsoleClear?.();
        this.disposeConsoleClear = null;
        for (const controller of this.abortControllers.values()) controller.abort();
        if (this.messageHandlerRef) {
            window.removeEventListener('message', this.messageHandlerRef);
            this.messageHandlerRef = null;
        }
        if (this.iframe) {
            this.iframe.remove();
        }
        this.closeActiveStreams();
        this.instanceRegistry.clear();
        for (const pending of this.pendingCallbacks.values()) {
            pending.dispose();
            pending.reject(new Error('Plugin sandbox terminated'));
        }
        this.pendingCallbacks.clear();
        this.abortControllers.clear();
        this.callbackWrapperCache.clear();
        this.activityListener?.();
        this.activityListener = null;
    }
}
