import {
    writeFile,
    BaseDirectory,
    readFile,
    exists,
    mkdir,
    readDir
} from "@tauri-apps/plugin-fs"
import { changeFullscreen, sleep } from "./util"
import { convertFileSrc } from "@tauri-apps/api/core"
import { v4 as uuidv4, v4 } from 'uuid';
import { appDataDir, join } from "@tauri-apps/api/path";
import { get } from "svelte/store";
import { open } from '@tauri-apps/plugin-shell'
import { openUrl } from '@tauri-apps/plugin-opener'
import streamSaver from 'streamsaver';
import { type Database, defaultSdDataFunc, getDatabase, getCurrentCharacter, type character, type groupChat, appSubVer } from "./storage/database.svelte";
import versionData from "../../version.json";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { MobileGUI, botMakerMode, selectedCharID, loadedStore, DBState, LoadingStatusState, selIdState, ReloadGUIPointer, bodyIntercepterStore } from "./stores.svelte";
import { loadPlugins } from "./plugins/plugins.svelte";
import { alertConfirm, alertError, alertMd, alertNormal, alertNormalWait, alertSelect, alertTOS, waitAlert } from "./alert";
import { hasher } from "./parser/parser.svelte";
import { characterURLImport, hubURL, realmHubURL } from "./characterCards";
import { defaultJailbreak, defaultMainPrompt, oldJailbreak, oldMainPrompt } from "./storage/defaultPrompts";
import { loadRisuAccountData } from "./drive/accounter";
import { saveDbKei } from "./kei/backup";
import { decodeRisuSave } from "./storage/risuSave";
import { AutoStorage } from "./storage/autoStorage";
import { updateAnimationSpeed } from "./gui/animation";
import { updateColorScheme, updateTextThemeAndCSS } from "./gui/colorscheme";
import { save } from "@tauri-apps/plugin-dialog";
import { language } from "src/lang";
import { startObserveDom } from "./observer.svelte";
import { updateGuisize } from "./gui/guisize";
import { updateLorebooks } from "./characters";
import { initMobileGesture } from "./hotkey";
import { fetch as TauriHTTPFetch } from '@tauri-apps/plugin-http';
import { fetchTauriHttpStream } from './network/tauriHttpStream';
import { moduleUpdate } from "./process/modules";
import {
    listCharacterResources,
    listDatabaseRootResources,
    replaceCharacterResources,
    replaceDatabaseRootResources,
} from "./process/coldstorageData";
import { collectExactPluginStorageAssetReferences } from "./drive/backupAssets";
import { downloadIOSFile } from "./storage/iosFiles";
import { isTauriIOS, isTauri, isTauriMobile, isNodeServer } from "./platform";
import { isLocalNetworkUrl } from "./network/localNetwork";
import { decodeProxyJobWsChunk, formatProxyStreamErrorMessage, parseProxyJobWsEvent } from "./network/proxyJobWs";
import { getNodeServerProxyAuth } from "./storage/nodeStorage";
import { ByteBudgetLru } from "./util/byteBudgetLru";
import { getRuntimePerformanceBudgets, subscribeRuntimePerformanceProfile } from "./runtimePerformanceProfile";
import { checkCharOrder as repairDatabaseCharacterOrder } from "./storage/databasePreparation";
import {
    activateConversation,
    configurePersistentDataRuntime,
    fencePersistentNavigation,
    markPersistentDataDirty,
    replacePersistentDatabase,
} from "./storage/persistentDataRuntime.svelte";
import * as persistentDataRuntime from "./storage/persistentDataRuntime.svelte";
import {
    queryChatMessageTargetAt,
    queryChatMessageTargetById,
    resolveRetainedChatMessageTarget,
    type CapturedChatMessageTarget,
} from "./chatMessageUi";
import {
    createPersistentSaveObserverInstallation,
    installPersistentSaveNotifications,
} from "./storage/persistentSaveNotifications";
import { observePersistentSaveChanges } from './storage/persistentSaveObserver.svelte';
import { configureBlobStoreStorageProvider, readBlobForFacade, resolveBlobStore } from "./storage/platformBlobStore";
import { inferBlobMime } from "./storage/blobStore";
import { selectAssetSourceRoute } from "./storage/assetSourceRoute";
import { readActiveAsset, storeActiveAsset } from "./storage/accountAssetAccess";
import { AppendableBuffer } from "./appendableBuffer"
import { beginNavigationActivity } from './ui/navigationActivity'
import { yieldToUi } from './ui/yieldToUi'

export { AppendableBuffer } from "./appendableBuffer"

export const forageStorage = new AutoStorage()
configureBlobStoreStorageProvider(async () => {
    await forageStorage.Init()
    return forageStorage.realStorage as any
})

const appWindow = isTauri ? getCurrentWebviewWindow() : null

interface fetchLog {
    body: string
    header: string
    response: string
    success: boolean,
    date: string
    url: string
    responseType?: string
    chatId?: string
    status?: number
}

let fetchLog: fetchLog[] = []

export async function downloadFile(name: string, dat: Uint8Array | ArrayBuffer | string): Promise<boolean> {
    if (typeof (dat) === 'string') {
        dat = Buffer.from(dat, 'utf-8')
    }
    const data = new Uint8Array(dat)
    const downloadURL = (data: string, fileName: string) => {
        const a = document.createElement('a')
        a.href = data
        a.download = fileName
        document.body.appendChild(a)
        a.style.display = 'none'
        a.click()
        a.remove()
    }

    if (isTauriIOS) return downloadIOSFile(name, data);
    if (isTauriMobile) {
        // Android resolves the download directory to app private storage the user cannot browse,
        // so exports go through the system picker instead.
        const extension = name.includes('.') ? name.split('.').pop()! : 'bin'
        const target = await save({
            defaultPath: name,
            filters: [{ name: extension.toUpperCase(), extensions: [extension] }],
        })
        if (!target) {
            return false
        }
        await writeFile(target, data)
    }
    else if (isTauri) {
        await writeFile(name, data, { baseDir: BaseDirectory.Download })
    }
    else {
        const blob = new Blob([data], { type: 'application/octet-stream' })
        const url = URL.createObjectURL(blob)

        downloadURL(url, name)

        setTimeout(() => {
            URL.revokeObjectURL(url)
        }, 10000)
    }
    return true
}

let fileCache: {
    origin: string[], res: (Uint8Array | 'loading' | 'done')[]
} = {
    origin: [],
    res: []
}

function createBrowserAssetDataUrlCache() {
    return new ByteBudgetLru<string, string>(
        getRuntimePerformanceBudgets().browserAssetDataUrlCacheBytes,
        (_loc, dataUrl) => dataUrl.length,
        256,
    )
}

let browserAssetDataUrlCache = createBrowserAssetDataUrlCache()
subscribeRuntimePerformanceProfile(() => {
    browserAssetDataUrlCache = createBrowserAssetDataUrlCache()
})
const pendingBrowserAssetReads = new Map<string, Promise<string | null>>()
const pendingTauriAssetUrls = new Map<string, Promise<string | null>>()
const tauriAssetUrlCache = new ByteBudgetLru<string, string>(
    Number.POSITIVE_INFINITY,
    () => 0,
    256,
)

function buildAssetDataUrl(mime: string | undefined, data: Uint8Array): string {
    return `data:${mime || 'application/octet-stream'};base64,${Buffer.from(data).toString('base64')}`
}

/** Resolves a local blob to a data URL, or null when the local store misses it. */
async function readBrowserAssetDataUrl(loc: string): Promise<string | null> {
    const cached = browserAssetDataUrlCache.get(loc)
    if (cached !== undefined) return cached
    let pending = pendingBrowserAssetReads.get(loc)
    if (!pending) {
        pending = (async () => {
            const blobStore = loc.startsWith('assets/') ? await resolveBlobStore() : null
            const data = blobStore
                ? await blobStore.read(loc)
                : await forageStorage.getItem(loc) as unknown as Uint8Array
            if (!data) return null
            const metadata = await blobStore?.stat(loc)
            const dataUrl = buildAssetDataUrl(metadata?.mime, data)
            if (pendingBrowserAssetReads.get(loc) === pending) {
                browserAssetDataUrlCache.set(loc, dataUrl)
            }
            return dataUrl
        })()
        pendingBrowserAssetReads.set(loc, pending)
        const cleanup = () => {
            if (pendingBrowserAssetReads.get(loc) === pending) pendingBrowserAssetReads.delete(loc)
        }
        void pending.then(cleanup, cleanup)
    }
    return await pending
}

export function invalidateAssetSourceCache(key: string): void {
    tauriAssetUrlCache.delete(key)
    browserAssetDataUrlCache.delete(key)
    pendingBrowserAssetReads.delete(key)
    pendingTauriAssetUrls.delete(key)
}

/** Resolves a Tauri asset URL once per key, including concurrent lookups. */
async function resolveTauriAssetUrl(loc: string): Promise<string | null> {
    const cached = tauriAssetUrlCache.get(loc)
    if (cached !== undefined) return cached
    let pending = pendingTauriAssetUrls.get(loc)
    if (!pending) {
        pending = (async () => {
            const url = await (await resolveBlobStore()).resolveUrl(loc)
            if (url && pendingTauriAssetUrls.get(loc) === pending) {
                tauriAssetUrlCache.set(loc, url)
            }
            return url
        })()
        pendingTauriAssetUrls.set(loc, pending)
        const cleanup = () => {
            if (pendingTauriAssetUrls.get(loc) === pending) pendingTauriAssetUrls.delete(loc)
        }
        void pending.then(cleanup, cleanup)
    }
    return await pending
}

let checkedPaths: string[] = []

/**
 * Gets the source URL of a file.
 * 
 * @param {string} loc - The location of the file.
 * @returns {Promise<string>} - A promise that resolves to the source URL of the file.
 */
export async function getFileSrc(loc: string) {
    if (!isTauri) await forageStorage.Init()
    const route = selectAssetSourceRoute(loc, isTauri, forageStorage.isAccount)
    if (route === 'account') {
        // Freshly imported assets only exist locally until the next publish
        // uploads them, so the local blob store wins over the hub URL.
        if (loc.startsWith('assets/')) {
            try {
                const local = isTauri
                    ? await resolveTauriAssetUrl(loc)
                    : await readBrowserAssetDataUrl(loc)
                if (local) return local
            } catch (error) {
                console.error(error)
            }
        }
        // `/rs/` is classified as a Realm path (scripts/realmBlocklist.mjs), so it
        // must come from realmHubURL for the agent-mode swap to cover it.
        return realmHubURL + `/rs/` + loc
    }
    if (route === 'tauri-asset') {
        const url = await resolveTauriAssetUrl(loc)
        if (!url) console.error(new Error(`Missing asset: ${loc}`))
        return url ?? ''
    }
    if (route === 'tauri-path') {
        return convertFileSrc(loc)
    }
    const blobStore = loc.startsWith('assets/') ? await resolveBlobStore() : null
    const readLocalFile = async () => blobStore
        ? await blobStore.read(loc)
        : await forageStorage.getItem(loc) as unknown as Uint8Array
    try {
        if (usingSw) {
            const encoded = Buffer.from(loc, 'utf-8').toString('hex')
            let ind = fileCache.origin.indexOf(loc)
            if (ind === -1) {
                ind = fileCache.origin.length
                fileCache.origin.push(loc)
                fileCache.res.push('loading')
                try {
                    const hasCache: boolean = (await (await fetch("/sw/check/" + encoded)).json()).able
                    if (hasCache) {
                        fileCache.res[ind] = 'done'
                        return "/sw/img/" + encoded
                    }
                    else {
                        const f = await readLocalFile()
                        if (!f) throw new Error(`Missing asset: ${loc}`)
                        await fetch("/sw/register/" + encoded, {
                            method: "POST",
                            body: f as any
                        })
                        fileCache.res[ind] = 'done'
                        await sleep(10)
                    }
                    return "/sw/img/" + encoded
                } catch (error) {

                }
            }
            else {
                const f = fileCache.res[ind]
                if (f === 'loading') {
                    while (fileCache.res[ind] === 'loading') {
                        await sleep(10)
                    }
                }
                return "/sw/img/" + encoded
            }
        }
        else {
            const dataUrl = await readBrowserAssetDataUrl(loc)
            if (dataUrl === null) throw new Error(`Missing asset: ${loc}`)
            return dataUrl
        }
    } catch (error) {
        console.error(error)
        return ''
    }
}

let appDataDirPath = ''

/**
 * Reads an image file and returns its data.
 * 
 * @param {string} data - The path to the image file.
 * @returns {Promise<Uint8Array>} - A promise that resolves to the data of the image file.
 */
export async function readImage(data: string) {
    if (!isTauri) await forageStorage.Init()
    const isAsset = data.startsWith('assets/') && data.length > 'assets/'.length
    if (isAsset) {
        return await readActiveAsset(await resolveBlobStore(), data, {
            officialAccount: forageStorage.isAccount,
            tauri: isTauri,
        })
    }
    if (isTauri) {
        if (data.startsWith('assets')) {
            if (appDataDirPath === '') {
                appDataDirPath = await appDataDir();
            }
            return await readFile(await join(appDataDirPath, data))
        }
        return await readFile(data)
    }
    else {
        return (await forageStorage.getItem(data) as unknown as Uint8Array)
    }
}

/**
 * Saves an asset file with the given data, custom ID, and file name.
 * 
 * @param {Uint8Array} data - The data of the asset file.
 * @param {string} [customId=''] - The custom ID for the asset file.
 * @param {string} [fileName=''] - The name of the asset file.
 * @returns {Promise<string>} - A promise that resolves to the path of the saved asset file.
 */
export async function saveAsset(data: Uint8Array, customId: string = '', fileName: string = '') {
    if (!isTauri) await forageStorage.Init()
    let id = ''
    if (customId !== '') {
        id = customId
    }
    else {
        try {
            id = await hasher(data)
        } catch (error) {
            id = uuidv4()
        }
    }
    let fileExtension: string = 'png'
    if (fileName && fileName.split('.').length > 0) {
        fileExtension = fileName.split('.').pop()
    }
    const form = `assets/${id}.${fileExtension}`
    await storeActiveAsset(await resolveBlobStore(), form, data, {
        kind: 'asset',
        mime: '',
        name: fileName || `${id}.${fileExtension}`,
        ext: fileExtension,
    })
    tauriAssetUrlCache.delete(form)
    pendingTauriAssetUrls.delete(form)
    pendingBrowserAssetReads.delete(form)
    if (browserAssetDataUrlCache.get(form) !== undefined) {
        browserAssetDataUrlCache.set(form, buildAssetDataUrl(inferBlobMime('', fileExtension), data))
    }
    return form
}

/**
 * Loads an asset file with the given ID.
 * 
 * @param {string} id - The ID of the asset file to load.
 * @returns {Promise<Uint8Array>} - A promise that resolves to the data of the loaded asset file.
 */
export async function loadAsset(id: string) {
    if (!isTauri) await forageStorage.Init()
    const isAsset = id.startsWith('assets/') && id.length > 'assets/'.length
    if (isAsset) {
        return await readActiveAsset(await resolveBlobStore(), id, {
            officialAccount: forageStorage.isAccount,
            tauri: isTauri,
        })
    }
    return await readBlobForFacade(await resolveBlobStore(), id, isTauri)
}

/**
 * Saves the current state of the database.
 */
export let saving = $state({
    state: false
})

const persistentSaveObserverInstallation = createPersistentSaveObserverInstallation()

export async function saveDb() {
    persistentSaveObserverInstallation.install(() => {
        const channel = window.BroadcastChannel ? new BroadcastChannel('risu-db') : null
        const disposeNotifications = installPersistentSaveNotifications({
            sessionId: v4(),
            channel,
            configureRuntime: configurePersistentDataRuntime,
            showForeignRevisionWarning: () => {
                void alertNormalWait(language.activeTabChange).then(() => location.reload())
            },
            setSaving: (value) => {
                saving.state = value
            },
            reportError: (error) => alertError(error instanceof Error ? error : String(error)),
            onSaveCommitted: saveDbKei,
        })
        const disposeEffects = observePersistentSaveChanges({
            readDatabase: () => DBState.db,
            readSelectedCharacter: () => DBState.db.characters?.[selIdState.selId] ?? null,
            markDirty: markPersistentDataDirty,
        })
        return () => {
            disposeEffects()
            disposeNotifications()
            saving.state = false
        }
    })
}

let usingSw = false

export function setUsingSw(value: boolean) {
    usingSw = value
}

/**
 * Retrieves fetch data for a given chat ID.
 * 
 * @param {string} id - The chat ID to search for in the fetch log.
 * @returns {fetchLog | null} - The fetch log entry if found, otherwise null.
 */
export function getFetchData(id: string) {
    for (const log of fetchLog) {
        if (log.chatId === id) {
            return log;
        }
    }
    return null;
}

const knownHostes = ["localhost", "127.0.0.1", "0.0.0.0"];
const defaultProxyJobHeartbeatSec = 15;

function getProxy2Url() {
    return !isTauri && !isNodeServer ? `${hubURL}/proxy2` : `/proxy2`;
}

function getProxyStreamJobBaseUrl() {
    return isNodeServer ? '' : `${hubURL}`;
}

function buildTimeoutSignal(originalSignal?: AbortSignal, timeoutMs?: number) {
    if (!timeoutMs || timeoutMs <= 0) {
        return {
            signal: originalSignal,
            cleanup: () => { /* no-op */ }
        };
    }

    const controller = new AbortController();
    const onAbort = () => controller.abort();
    if (originalSignal) {
        if (originalSignal.aborted) {
            controller.abort();
        }
        else {
            originalSignal.addEventListener('abort', onAbort, { once: true });
        }
    }

    const timeoutId = setTimeout(() => controller.abort(), timeoutMs);

    return {
        signal: controller.signal,
        cleanup: () => {
            clearTimeout(timeoutId);
            originalSignal?.removeEventListener('abort', onAbort);
        }
    };
}

/**
 * Interface representing the arguments for the global fetch function.
 * 
 * @interface GlobalFetchArgs
 * @property {boolean} [plainFetchForce] - Whether to force plain fetch.
 * @property {any} [body] - The body of the request.
 * @property {{ [key: string]: string }} [headers] - The headers of the request.
 * @property {boolean} [rawResponse] - Whether to return the raw response.
 * @property {'POST' | 'GET'} [method] - The HTTP method to use.
 * @property {AbortSignal} [abortSignal] - The abort signal to cancel the request.
 * @property {boolean} [useRisuToken] - Whether to use the Risu token.
 * @property {string} [chatId] - The chat ID associated with the request.
 */
export interface GlobalFetchArgs {
    plainFetchForce?: boolean;
    plainFetchDeforce?: boolean;
    body?: any;
    headers?: { [key: string]: string };
    rawResponse?: boolean;
    method?: 'POST' | 'GET';
    abortSignal?: AbortSignal;
    useRisuToken?: boolean;
    chatId?: string;
    interceptor?: string;
    requestTimeoutMs?: number;
    networkRoute?: 'auto' | 'local_network';
}

/**
 * Interface representing the result of the global fetch function.
 * 
 * @interface GlobalFetchResult
 * @property {boolean} ok - Whether the request was successful.
 * @property {any} data - The data returned from the request.
 * @property {{ [key: string]: string }} headers - The headers returned from the request.
 */
interface GlobalFetchResult {
    ok: boolean;
    data: any;
    headers: { [key: string]: string };
    status: number;
}

/**
 * Adds a fetch log entry.
 * 
 * @param {Object} arg - The arguments for the fetch log entry.
 * @param {any} arg.body - The body of the request.
 * @param {{ [key: string]: string }} [arg.headers] - The headers of the request.
 * @param {any} arg.response - The response from the request.
 * @param {boolean} arg.success - Whether the request was successful.
 * @param {string} arg.url - The URL of the request.
 * @param {string} [arg.resType] - The response type.
 * @param {string} [arg.chatId] - The chat ID associated with the request.
 * @returns {number} - The index of the added fetch log entry.
 */
export function addFetchLog(arg: {
    body: any,
    headers?: { [key: string]: string },
    response: any,
    success: boolean,
    url: string,
    resType?: string,
    chatId?: string,
    status?: number
}): number {
    fetchLog.unshift({
        body: typeof (arg.body) === 'string' ? arg.body : JSON.stringify(arg.body, null, 2),
        header: JSON.stringify(arg.headers ?? {}, null, 2),
        response: typeof (arg.response) === 'string' ? arg.response : JSON.stringify(arg.response, null, 2),
        responseType: arg.resType ?? 'json',
        success: arg.success,
        date: (new Date()).toLocaleTimeString(),
        url: arg.url,
        chatId: arg.chatId,
        status: arg.status
    });
    return 0;
}

/**
 * Performs a global fetch request.
 * 
 * @param {string} url - The URL to fetch.
 * @param {GlobalFetchArgs} [arg={}] - The arguments for the fetch request.
 * @returns {Promise<GlobalFetchResult>} - The result of the fetch request.
 */
export async function globalFetch(url: string, arg: GlobalFetchArgs = {}): Promise<GlobalFetchResult> {
    try {
        const db = getDatabase();
        if (arg.abortSignal?.aborted) { return { ok: false, data: 'aborted', headers: {}, status: 400 }; }

        const urlHost = new URL(url).hostname
        const useLocalNetworkRoute = isLocalNetworkUrl(url)
            && (arg.networkRoute === 'local_network' || (!isTauri && !isNodeServer))
        const forcePlainFetch = ((knownHostes.includes(urlHost) && !isTauri) || db.usePlainFetch || arg.plainFetchForce) && !arg.plainFetchDeforce && !useLocalNetworkRoute

        if(arg.interceptor){
            for (const interceptor of bodyIntercepterStore) {
                try {
                    arg.body = await interceptor.callback(arg.body, arg.interceptor) || arg.body
                }
                catch (e) {
                    console.error(e)
                }
            }
        }

        const timeoutSignal = buildTimeoutSignal(arg.abortSignal, arg.requestTimeoutMs)
        const requestArg = timeoutSignal.signal === arg.abortSignal
            ? arg
            : { ...arg, abortSignal: timeoutSignal.signal }

        try {
            if (useLocalNetworkRoute) {
                if (isTauri) {
                    return await fetchWithTauri(url, requestArg);
                }
                if (!isNodeServer) {
                    return window.userScriptFetch
                        ? await fetchWithUSFetch(url, requestArg)
                        : await fetchWithPlainFetch(url, requestArg);
                }
                return await fetchWithProxy(url, requestArg);
            }
            if (forcePlainFetch) {
                return await fetchWithPlainFetch(url, requestArg);
            }
            //userScriptFetch is provided by userscript
            if (window.userScriptFetch) {
                return await fetchWithUSFetch(url, requestArg);
            }
            if (isTauri) {
                return await fetchWithTauri(url, requestArg);
            }
            return await fetchWithProxy(url, requestArg);
        } finally {
            timeoutSignal.cleanup();
        }

    } catch (error) {
        console.error(error);
        return { ok: false, data: `${error}`, headers: {}, status: 400 };
    }
}

/**
 * Adds a fetch log entry in the global fetch log.
 * 
 * @param {any} response - The response data.
 * @param {boolean} success - Indicates if the fetch was successful.
 * @param {string} url - The URL of the fetch request.
 * @param {GlobalFetchArgs} arg - The arguments for the fetch request.
 */
function addFetchLogInGlobalFetch(response: any, success: boolean, url: string, arg: GlobalFetchArgs, status?: number) {
    try {
        fetchLog.unshift({
            body: JSON.stringify(arg.body, null, 2),
            header: JSON.stringify(arg.headers ?? {}, null, 2),
            response: JSON.stringify(response, null, 2),
            success: success,
            date: (new Date()).toLocaleTimeString(),
            url: url,
            chatId: arg.chatId,
            status: status
        })
    }
    catch {
        fetchLog.unshift({
            body: JSON.stringify(arg.body, null, 2),
            header: JSON.stringify(arg.headers ?? {}, null, 2),
            response: `${response}`,
            success: success,
            date: (new Date()).toLocaleTimeString(),
            url: url,
            chatId: arg.chatId,
            status: status
        })
    }

    if (fetchLog.length > 20) {
        fetchLog.pop()
    }
}

/**
 * Performs a fetch request using plain fetch.
 * 
 * @param {string} url - The URL to fetch.
 * @param {GlobalFetchArgs} arg - The arguments for the fetch request.
 * @returns {Promise<GlobalFetchResult>} - The result of the fetch request.
 */
async function fetchWithPlainFetch(url: string, arg: GlobalFetchArgs): Promise<GlobalFetchResult> {
    try {
        const headers = { 'Content-Type': 'application/json', ...arg.headers };
        const response = await fetch(new URL(url), { body: JSON.stringify(arg.body), headers, method: arg.method ?? "POST", signal: arg.abortSignal });
        const data = arg.rawResponse ? new Uint8Array(await response.arrayBuffer()) : await response.json();
        const ok = response.ok && response.status >= 200 && response.status < 300;
        addFetchLogInGlobalFetch(data, ok, url, arg, response.status);
        return { ok, data, headers: Object.fromEntries(response.headers), status: response.status };
    } catch (error) {
        return { ok: false, data: `${error}`, headers: {}, status: 400 };
    }
}

/**
 * Performs a fetch request using userscript provided fetch.
 * 
 * @param {string} url - The URL to fetch.
 * @param {GlobalFetchArgs} arg - The arguments for the fetch request.
 * @returns {Promise<GlobalFetchResult>} - The result of the fetch request.
 */
async function fetchWithUSFetch(url: string, arg: GlobalFetchArgs): Promise<GlobalFetchResult> {
    try {
        const headers = { 'Content-Type': 'application/json', ...arg.headers };
        const response = await userScriptFetch(url, { body: JSON.stringify(arg.body), headers, method: arg.method ?? "POST", signal: arg.abortSignal });
        const data = arg.rawResponse ? new Uint8Array(await response.arrayBuffer()) : await response.json();
        const ok = response.ok && response.status >= 200 && response.status < 300;
        addFetchLogInGlobalFetch(data, ok, url, arg, response.status);
        return { ok, data, headers: Object.fromEntries(response.headers), status: response.status };
    } catch (error) {
        return { ok: false, data: `${error}`, headers: {}, status: 400 };
    }
}

/**
 * Performs a fetch request using Tauri.
 * 
 * @param {string} url - The URL to fetch.
 * @param {GlobalFetchArgs} arg - The arguments for the fetch request.
 * @returns {Promise<GlobalFetchResult>} - The result of the fetch request.
 */
async function fetchWithTauri(url: string, arg: GlobalFetchArgs): Promise<GlobalFetchResult> {
    try {
        const headers = { 'Content-Type': 'application/json', ...arg.headers };
        const response = await TauriHTTPFetch(new URL(url), { body: JSON.stringify(arg.body), headers, method: arg.method ?? "POST", signal: arg.abortSignal });
        const data = arg.rawResponse ? new Uint8Array(await response.arrayBuffer()) : await response.json();
        const ok = response.status >= 200 && response.status < 300;
        addFetchLogInGlobalFetch(data, ok, url, arg, response.status);
        return { ok, data, headers: Object.fromEntries(response.headers), status: response.status };
    } catch (error) {
        return { ok: false, data: `${error}`, headers: {}, status: 400 };
    }
}

/**
 * Performs a fetch request using a proxy.
 * 
 * @param {string} url - The URL to fetch.
 * @param {GlobalFetchArgs} arg - The arguments for the fetch request.
 * @returns {Promise<GlobalFetchResult>} - The result of the fetch request.
 */
async function fetchWithProxy(url: string, arg: GlobalFetchArgs): Promise<GlobalFetchResult> {
    try {
        const furl = getProxy2Url();
        arg.headers ??= {};
        arg.headers["Content-Type"] ??= arg.body instanceof URLSearchParams ? "application/x-www-form-urlencoded" : "application/json";
        const nodeProxyAuth = isNodeServer ? await getNodeServerProxyAuth() : null;
        const headers = {
            "risu-header": encodeURIComponent(JSON.stringify(arg.headers)),
            "risu-url": encodeURIComponent(url),
            "Content-Type": arg.body instanceof URLSearchParams ? "application/x-www-form-urlencoded" : "application/json",
            ...(arg.useRisuToken && { "x-risu-tk": "use" }),
            ...(arg.requestTimeoutMs && { "risu-timeout-ms": Math.max(1, Math.floor(arg.requestTimeoutMs)).toString() }),
            ...(nodeProxyAuth && { "risu-auth": nodeProxyAuth }),
            ...(DBState?.db?.requestLocation && { "risu-location": DBState.db.requestLocation }),
        };

        const body = arg.body instanceof URLSearchParams ? arg.body.toString() : JSON.stringify(arg.body);

        const response = await fetch(furl, { body, headers, method: arg.method ?? "POST", signal: arg.abortSignal });
        const isSuccess = response.ok && response.status >= 200 && response.status < 300;

        if (arg.rawResponse) {
            const data = new Uint8Array(await response.arrayBuffer());
            addFetchLogInGlobalFetch("Uint8Array Response", isSuccess, url, arg, response.status);
            return { ok: isSuccess, data, headers: Object.fromEntries(response.headers), status: response.status };
        }

        const text = await response.text();
        try {
            const data = JSON.parse(text);
            addFetchLogInGlobalFetch(data, isSuccess, url, arg, response.status);
            return { ok: isSuccess, data, headers: Object.fromEntries(response.headers), status: response.status };
        } catch (error) {
            const errorMsg = text.startsWith('<!DOCTYPE') ? "Responded HTML. Is your URL, API key, and password correct?" : text;
            addFetchLogInGlobalFetch(text, false, url, arg, response.status);
            return { ok: false, data: errorMsg, headers: Object.fromEntries(response.headers), status: response.status };
        }
    } catch (error) {
        return { ok: false, data: `${error}`, headers: {}, status: 400 };
    }
}

/**
 * Regular expression to match backslashes.
 * 
 * @constant {RegExp}
 */
const re = /\\/g;

/**
 * Gets the basename of a given path.
 * 
 * @param {string} data - The path to get the basename from.
 * @returns {string} - The basename of the path.
 */
export function getBasename(data: string) {
    const splited = data.replace(re, '/').split('/');
    const lasts = splited[splited.length - 1];
    return lasts;
}

export async function getUncleanables(db: Database, uptype: 'basename' | 'pure' = 'basename') {
    return getUncleanablesSync(db, uptype);
}

/**
 * Retrieves uncleanable resources from the database.
 * 
 * @param {Database} db - The database to retrieve uncleanable resources from.
 * @param {'basename'|'pure'} [uptype='basename'] - The type of uncleanable resources to retrieve.
 * @returns {Promise<string[]>} - An array of uncleanable resources.
 */
export function getUncleanablesSync(db: Database, uptype: 'basename' | 'pure' = 'basename', options?:{
    chars: (character|groupChat)[],
}) {
    const uncleanable = new Set<string>();

    /**
     * Adds a resource to the uncleanable list if it is not already included.
     * 
     * @param {string} data - The resource to add.
     */
    function addUncleanable(data: string) {
        if (!data) {
            return;
        }
        if (data === '') {
            return;
        }
        const bn = uptype === 'basename' ? getBasename(data) : data;
        uncleanable.add(bn);
    }

    const chars = options?.chars ?? db.characters
    for (const resource of listDatabaseRootResources(db)) {
        addUncleanable(resource)
    }
    for (const cha of chars) {
        for (const resource of listCharacterResources(cha)) {
            addUncleanable(resource)
        }
    }
    // Assets referenced only from plugin storage are backed up as required
    // data; cleanup must not treat them as orphans.
    for (const resource of collectExactPluginStorageAssetReferences(db.pluginCustomStorage ?? {})) {
        addUncleanable(resource)
    }
    return Array.from(uncleanable);
}


/**
 * Replaces database resources with the provided replacer object.
 * 
 * @param {Database} db - The database object containing resources to be replaced.
 * @param {{[key: string]: string}} replacer - An object mapping original resource keys to their replacements.
 * @returns {Database} - The updated database object with replaced resources.
 */
export function replaceDbResources(db: Database, replacer: { [key: string]: string }): Database {
    const { characters, ...root } = db
    return {
        ...replaceDatabaseRootResources(root, replacer),
        characters: characters.map((character) => (
            replaceCharacterResources(character, replacer)
        )),
    }
}

/**
 * Checks and updates the character order in the database.
 * Ensures that all characters are properly ordered and removes any invalid entries.
 */
export function checkCharOrder(database: Database = DBState.db): Database {
    return repairDatabaseCharacterOrder(database)
}

/**
 * Retrieves the request log as a formatted string.
 * 
 * @returns {string} The formatted request log.
 */
export function getRequestLog() {
    let logString = ''
    const b = '\n\`\`\`json\n'
    const bend = '\n\`\`\`\n'

    for (const log of fetchLog) {
        logString += `## ${log.date}\n\n* Request URL\n\n${b}${log.url}${bend}\n\n* Request Body\n\n${b}${log.body}${bend}\n\n* Request Header\n\n${b}${log.header}${bend}\n\n`
            + `* Response Body\n\n${b}${log.response}${bend}\n\n* Response Success\n\n${b}${log.success}${bend}\n\n`
    }
    return logString
}

/**
 * Retrieves the fetch logs array.
 *
 * @returns {fetchLog[]} The fetch logs array.
 */
export function getFetchLogs() {
    return fetchLog
}

/**
 * Opens a URL in the appropriate environment.
 * 
 * @param {string} url - The URL to open.
 */
export function openURL(url: string) {
    if (isTauriIOS) {
        void openUrl(url).catch((error) => alertError(String(error)))
    }
    else if (isTauri) {
        open(url)
    }
    else {
        window.open(url, "_blank")
    }
}

/**
 * Converts FormData to a URL-encoded string.
 * 
 * @param {FormData} formData - The FormData to convert.
 * @returns {string} The URL-encoded string.
 */
function formDataToString(formData: FormData): string {
    const params: string[] = [];

    for (const [name, value] of formData.entries()) {
        params.push(`${encodeURIComponent(name)}=${encodeURIComponent(value.toString())}`);
    }

    return params.join('&');
}

/**
 * A writer class for Tauri environment.
 */
const TAURI_WRITER_FLUSH_BYTES = 4 * 1024 * 1024

export class TauriWriter {
    path: string
    firstWrite: boolean = true
    private pending: Uint8Array[] = []
    private pendingBytes = 0
    private aborted = false

    /**
     * Creates an instance of TauriWriter.
     * 
     * @param {string} path - The file path to write to.
     */
    constructor(path: string) {
        this.path = path
    }

    /**
     * Buffers data and appends it in large blocks, because every append reopens the destination
     * and an Android content URI makes that round trip expensive.
     */
    async write(data: Uint8Array) {
        if (this.aborted) throw new Error('Cannot write to an aborted Tauri writer')
        this.pending.push(data.slice())
        this.pendingBytes += data.byteLength
        if (this.pendingBytes >= TAURI_WRITER_FLUSH_BYTES) await this.flush()
    }

    /**
     * Flushes any buffered data.
     */
    async close() {
        if (this.aborted) throw new Error('Cannot close an aborted Tauri writer')
        await this.flush()
    }

    async abort() {
        if (this.aborted) return
        this.aborted = true
        this.pending = []
        this.pendingBytes = 0
    }

    private async flush() {
        if (this.pending.length === 0) return
        const block = new Uint8Array(this.pendingBytes)
        let offset = 0
        for (const chunk of this.pending) {
            block.set(chunk, offset)
            offset += chunk.byteLength
        }
        this.pending = []
        this.pendingBytes = 0
        await writeFile(this.path, block, { append: !this.firstWrite })
        this.firstWrite = false
    }
}


/**
 * Class representing a local writer.
 */
export class LocalWriter {
    writer: WritableStreamDefaultWriter | TauriWriter

    /**
     * Initializes the writer.
     * 
     * @param {string} [name='Binary'] - The name of the file.
     * @param {string[]} [ext=['bin']] - The file extensions.
     * @returns {Promise<boolean>} - A promise that resolves to a boolean indicating success.
     */
    async init(name = 'Binary', ext = ['bin'], defaultName?: string): Promise<boolean> {
        if (isTauri) {
            // Android never appends the filter extension, so the suggested name has to carry it.
            const filePath = await save({
                defaultPath: defaultName ?? `${name}.${ext[0] ?? 'bin'}`,
                filters: [{
                    name: name,
                    extensions: ext
                }]
            });
            if (!filePath) {
                return false
            }
            this.writer = new TauriWriter(filePath)
            return true
        }
        const writableStream = streamSaver.createWriteStream(defaultName ?? `${name}.${ext[0]}`)
        this.writer = writableStream.getWriter()
        return true
    }

    /**
     * Writes backup data to the file.
     * 
     * @param {string} name - The name of the backup.
     * @param {Uint8Array} data - The data to write.
     */
    async writeBackup(name: string, data: Uint8Array): Promise<void> {
        const encodedName = new TextEncoder().encode(getBasename(name))
        const record = new Uint8Array(8 + encodedName.byteLength + data.byteLength)
        const header = new DataView(record.buffer)
        header.setUint32(0, encodedName.byteLength, true)
        record.set(encodedName, 4)
        header.setUint32(4 + encodedName.byteLength, data.byteLength, true)
        record.set(data, 8 + encodedName.byteLength)
        await this.writer.write(record)
    }

    async writeBackupStream(
        name: string,
        byteLength: number,
        chunks: AsyncIterable<Uint8Array>,
    ): Promise<void> {
        if (!Number.isSafeInteger(byteLength) || byteLength < 0 || byteLength > 0xffffffff) {
            throw new Error('Backup entry length is outside the supported range')
        }
        const encodedName = new TextEncoder().encode(getBasename(name))
        const headerBytes = new Uint8Array(8 + encodedName.byteLength)
        const header = new DataView(headerBytes.buffer)
        header.setUint32(0, encodedName.byteLength, true)
        headerBytes.set(encodedName, 4)
        header.setUint32(4 + encodedName.byteLength, byteLength, true)
        await this.writer.write(headerBytes)

        let written = 0
        for await (const chunk of chunks) {
            if (written + chunk.byteLength > byteLength) {
                throw new Error('Backup entry stream exceeded its declared length')
            }
            if (chunk.byteLength > 0) await this.writer.write(chunk)
            written += chunk.byteLength
        }
        if (written !== byteLength) {
            throw new Error('Backup entry stream ended before its declared length')
        }
    }

    /**
     * Writes data to the file.
     * 
     * @param {Uint8Array} data - The data to write.
     */
    async write(data: Uint8Array): Promise<void> {
        await this.writer.write(data)
    }

    /**
     * Closes the writer.
     */
    async close(): Promise<void> {
        await this.writer.close()
    }

    async abort(): Promise<void> {
        const abortable = this.writer as typeof this.writer & {
            abort?: () => void | Promise<void>
        }
        if (!abortable.abort) throw new Error('Local writer does not support abort')
        await abortable.abort()
    }
}

/**
 * Class representing a virtual writer.
 */
export class VirtualWriter {
    buf = new AppendableBuffer()

    /**
     * Writes data to the buffer.
     * 
     * @param {Uint8Array} data - The data to write.
     */
    write(data: Uint8Array): void {
        this.buf.append(data)
    }

    /**
     * Closes the writer. (No operation for VirtualWriter)
     */
    close(): void {
        // do nothing
    }
}

/**
 * Pipes the fetch log to a readable stream.
 * @param {number} fetchLogIndex - The index of the fetch log.
 * @param {ReadableStream<Uint8Array>} readableStream - The readable stream to pipe.
 * @returns {ReadableStream<Uint8Array>} - The new readable stream.
 */
const pipeFetchLog = (fetchLogIndex: number, readableStream: ReadableStream<Uint8Array>) => {
    
    const splited = readableStream.tee();
    
    (async () => {
        const text = await (new Response(splited[0])).text()
        fetchLog[fetchLogIndex].response = text
    })()
    
    return splited[1]
}

async function fetchViaProxyJobWs(url: string, arg: {
    body: Uint8Array,
    headers?: { [key: string]: string },
    method: "POST" | "GET" | "PUT" | "DELETE",
    signal?: AbortSignal,
    requestTimeoutMs?: number,
    chatId?: string,
    fetchLogIndex?: number | null
}): Promise<Response> {
    const auth = await getNodeServerProxyAuth();

    const requestSignal = arg.signal;
    const baseUrl = getProxyStreamJobBaseUrl();

    let jobId = '';
    const createRes = await fetch(`${baseUrl}/proxy-stream-jobs`, {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
            'risu-auth': auth
        },
        body: JSON.stringify({
            url,
            method: arg.method,
            headers: arg.headers ?? {},
            bodyBase64: Buffer.from(arg.body).toString('base64'),
            timeoutMs: arg.requestTimeoutMs,
            heartbeatSec: defaultProxyJobHeartbeatSec
        }),
        signal: requestSignal
    });

    if (!createRes.ok) {
        const errText = await createRes.text();
        throw new Error(`Proxy stream job creation failed: ${createRes.status} ${errText}`);
    }

    const created = await createRes.json() as { jobId?: string };
    if (!created.jobId) {
        throw new Error('Proxy stream job creation returned no jobId');
    }
    jobId = created.jobId;

    const wsProtocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${wsProtocol}//${location.host}/proxy-stream-jobs/${encodeURIComponent(jobId)}/ws?risu-auth=${encodeURIComponent(auth)}`;

    let headersReady = false;
    let status = 200;
    let responseHeaders: HeadersInit = { 'content-type': 'text/event-stream' };
    let settled = false;
    let resolveHeaders: () => void = () => {};
    const waitHeaders = new Promise<void>((resolve) => {
        resolveHeaders = resolve;
    });
    let streamController: ReadableStreamDefaultController<Uint8Array> | null = null;
    const encoder = new TextEncoder();

    const ws = new WebSocket(wsUrl);
    const readable = new ReadableStream<Uint8Array>({
        start(controller) {
            streamController = controller;
        },
        cancel() {
            try {
                ws.close();
            } catch {
                // no-op
            }
        }
    });
    const pipedReadable = arg.fetchLogIndex != null ? pipeFetchLog(arg.fetchLogIndex, readable) : readable;

    const ensureHeadersReady = () => {
        if (!headersReady) {
            headersReady = true;
            resolveHeaders();
        }
    };

    const closeAndEnd = () => {
        if (settled) {
            return;
        }
        settled = true;
        if (streamController) {
            try {
                streamController.close();
            } catch {
                // no-op
            }
        }
        try {
            ws.close();
        } catch {
            // no-op
        }
    };

    ws.onmessage = (event) => {
        const parsed = parseProxyJobWsEvent(typeof event.data === 'string' ? event.data : '');
        if (!parsed || !streamController) {
            return;
        }
        switch (parsed.type) {
            case 'job_accepted':
            case 'ping':
                return;
            case 'upstream_headers':
                status = parsed.status;
                responseHeaders = parsed.headers ?? {};
                ensureHeadersReady();
                return;
            case 'chunk':
                ensureHeadersReady();
                streamController.enqueue(decodeProxyJobWsChunk(parsed.dataBase64));
                return;
            case 'error': {
                status = parsed.status ?? 502;
                responseHeaders = { 'content-type': 'text/plain; charset=utf-8' };
                ensureHeadersReady();
                const msg = formatProxyStreamErrorMessage(parsed.status, parsed.message);
                streamController.enqueue(encoder.encode(msg));
                closeAndEnd();
                return;
            }
            case 'done':
                ensureHeadersReady();
                closeAndEnd();
                return;
        }
    };

    ws.onerror = () => {
        if (!streamController) {
            return;
        }
        status = 502;
        responseHeaders = { 'content-type': 'text/plain; charset=utf-8' };
        ensureHeadersReady();
        streamController.enqueue(encoder.encode('Proxy WebSocket stream error'));
        closeAndEnd();
    };

    ws.onclose = () => {
        if (!headersReady) {
            status = 502;
            responseHeaders = { 'content-type': 'text/plain; charset=utf-8' };
            ensureHeadersReady();
        }
        closeAndEnd();
    };

    const abortHandler = () => {
        status = 499;
        responseHeaders = { 'content-type': 'text/plain; charset=utf-8' };
        ensureHeadersReady();
        if (streamController && !settled) {
            streamController.enqueue(encoder.encode('Aborted'));
        }
        void fetch(`${baseUrl}/proxy-stream-jobs/${encodeURIComponent(jobId)}`, {
            method: 'DELETE',
            headers: {
                'risu-auth': auth
            }
        }).catch(() => {});
        closeAndEnd();
    };
    if (requestSignal?.aborted) {
        abortHandler();
    }
    else {
        requestSignal?.addEventListener('abort', abortHandler, { once: true });
    }

    await waitHeaders;
    requestSignal?.removeEventListener('abort', abortHandler);
    return new Response(pipedReadable, {
        status,
        headers: new Headers(responseHeaders)
    });
}

/**
 * Fetches data from a given URL using native fetch or through a proxy.
 * @param {string} url - The URL to fetch data from.
 * @param {Object} arg - The arguments for the fetch request.
 * @param {string} arg.body - The body of the request.
 * @param {Object} [arg.headers] - The headers of the request.
 * @param {string} [arg.method="POST"] - The HTTP method of the request.
 * @param {AbortSignal} [arg.signal] - The signal to abort the request.
 * @param {boolean} [arg.useRisuTk] - Whether to use Risu token.
 * @param {string} [arg.chatId] - The chat ID associated with the request.
 * @returns {Promise<Object>} - A promise that resolves to an object containing the response body, headers, and status.
 * @returns {ReadableStream<Uint8Array>} body - The response body as a readable stream.
 * @returns {Headers} headers - The response headers.
 * @returns {number} status - The response status code.
 * @throws {Error} - Throws an error if the request is aborted or if there is an error in the response.
 */
export async function fetchNative(url: string, arg: {
    body?: string | Uint8Array | ArrayBuffer,
    headers?: { [key: string]: string },
    method?: string,
    signal?: AbortSignal,
    useRisuTk?: boolean,
    chatId?: string
    interceptor?: string
    logFetch?: boolean
    requestTimeoutMs?: number
    networkRoute?: 'auto' | 'local_network'
} = {}): Promise<Response> {

    const useInterceptor = !!arg.interceptor
    console.log(arg.body, 'body')
    // Keep implicit POST for existing callers that provide a body.
    arg = {
        ...arg,
        method: (
            arg.method ?? (arg.body === undefined ? 'GET' : 'POST')
        ).toUpperCase(),
    }

    let headers = arg.headers ?? {}
    let realBody: Uint8Array

    if (arg.body === undefined || arg.method === 'GET' || arg.method === 'HEAD') {
        realBody = undefined
    }
    else if (typeof arg.body === 'string') {
        let body: string = arg.body
        if(useInterceptor) {
            for (const interceptor of bodyIntercepterStore) {
                try {
                    body = await interceptor.callback(body, arg.interceptor) || body
                }
                catch (e) {
                    console.error(e)
                }
            }
        }
        realBody = new TextEncoder().encode(body)
    }
    else if (arg.body instanceof Uint8Array) {
        realBody = arg.body
    }
    else if (arg.body instanceof ArrayBuffer) {
        realBody = new Uint8Array(arg.body)
    }
    else {
        throw new Error('Invalid body type')
    }

    const db = getDatabase()
    const useLocalNetworkRoute = isLocalNetworkUrl(url)
        && (arg.networkRoute === 'local_network' || (!isTauri && !isNodeServer))
    let throughProxy = (!isTauri) && (!isNodeServer) && (!db.usePlainFetch)
    if (useLocalNetworkRoute) {
        if (isNodeServer) {
            throughProxy = true
        }
        else {
            throughProxy = false
        }
    }
    const route: 'userscript' | 'tauri' | 'proxy' | 'plain' =
        window.userScriptFetch && !throughProxy ? 'userscript'
            : isTauri ? 'tauri'
                : throughProxy ? 'proxy'
                    : 'plain'
    // The Tauri stream route manages its own composed timeout signal.
    const timeoutSignal = route === 'tauri' ? null : buildTimeoutSignal(arg.signal, arg.requestTimeoutMs)
    const requestSignal = timeoutSignal?.signal ?? arg.signal
    const shouldLogFetch = arg.logFetch ?? true
    let fetchLogIndex: number | null = null
    if (shouldLogFetch) {
        fetchLogIndex = addFetchLog({
            body: new TextDecoder().decode(realBody),
            headers: arg.headers,
            response: 'Streamed Fetch',
            success: true,
            url: url,
            resType: 'stream',
            chatId: arg.chatId,
        })
    }
    try {
        if (route === 'userscript') {
            return await window.userScriptFetch(url, {
            body: realBody as any,
            headers: headers,
            method: arg.method,
            signal: requestSignal
        })
        }
        else if (route === 'tauri') {
            const decoder = shouldLogFetch && fetchLogIndex !== null ? new TextDecoder() : null
            const responseParts: string[] = []
            return await fetchTauriHttpStream({
                url,
                method: arg.method,
                headers,
                body: realBody,
                signal: arg.signal,
                // The native route enforces the budget as an inactivity window, so a
                // long running generation keeps going while a silent one is released.
                idleTimeoutMs: arg.requestTimeoutMs,
                onChunk: decoder ? (chunk) => {
                    responseParts.push(decoder.decode(chunk, { stream: true }))
                } : undefined,
                onFinish: decoder ? () => {
                    responseParts.push(decoder.decode())
                    fetchLog[fetchLogIndex].response = responseParts.join('')
                } : undefined,
            })
        }
    else if (route === 'proxy') {
        const useProxyJobWs = isNodeServer
            && arg.interceptor === 'openai_streaming'
            && arg.method === 'POST'
            && useLocalNetworkRoute;
        const nodeProxyAuth = isNodeServer ? await getNodeServerProxyAuth() : null;

        if (useProxyJobWs) {
            try {
                return await fetchViaProxyJobWs(url, {
                    body: realBody,
                    headers,
                    method: 'POST',
                    signal: requestSignal,
                    requestTimeoutMs: arg.requestTimeoutMs,
                    chatId: arg.chatId,
                    fetchLogIndex
                });
            } catch (wsErr) {
                console.warn('[ProxyJobWS] fallback to /proxy2 due to error:', wsErr);
            }
        }

        const r = await fetch(getProxy2Url(), {
            body: realBody as any,
            headers: arg.useRisuTk ? {
                "risu-header": encodeURIComponent(JSON.stringify(headers)),
                "risu-url": encodeURIComponent(url),
                "Content-Type": "application/json",
                "x-risu-tk": "use",
                ...(arg.requestTimeoutMs && { "risu-timeout-ms": Math.max(1, Math.floor(arg.requestTimeoutMs)).toString() }),
                ...(nodeProxyAuth ? { "risu-auth": nodeProxyAuth } : {}),
                ...(DBState?.db?.requestLocation && { "risu-location": DBState.db.requestLocation }),
            } : {
                "risu-header": encodeURIComponent(JSON.stringify(headers)),
                "risu-url": encodeURIComponent(url),
                "Content-Type": "application/json",
                ...(arg.requestTimeoutMs && { "risu-timeout-ms": Math.max(1, Math.floor(arg.requestTimeoutMs)).toString() }),
                ...(nodeProxyAuth ? { "risu-auth": nodeProxyAuth } : {}),
                ...(DBState?.db?.requestLocation && { "risu-location": DBState.db.requestLocation }),
            },
            method: arg.method,
            signal: requestSignal
        })

        return new Response(r.body, {
            headers: r.headers,
            status: r.status
        })
    }
    else {
        return await fetch(url, {
            body: realBody as any,
            headers: headers,
            method: arg.method,
            signal: requestSignal,
        })
    }
    } finally {
        timeoutSignal?.cleanup()
    }
}

/**
 * Converts a ReadableStream of Uint8Array to a text string.
 * 
 * @param {ReadableStream<Uint8Array>} stream - The readable stream to convert.
 * @returns {Promise<string>} A promise that resolves to the text content of the stream.
 */
export function textifyReadableStream(stream: ReadableStream<Uint8Array>) {
    return new Response(stream).text()
}

/**
 * Toggles the fullscreen mode of the document.
 * If the document is currently in fullscreen mode, it exits fullscreen.
 * If the document is not in fullscreen mode, it requests fullscreen with navigation UI hidden.
 */
export function toggleFullscreen() {
    const fullscreenElement = document.fullscreenElement
    fullscreenElement ? document.exitFullscreen() : document.documentElement.requestFullscreen({
        navigationUI: "hide"
    })
}

/**
 * Removes non-Latin characters from a string, replaces multiple spaces with a single space, and trims the string.
 * 
 * @param {string} data - The input string to be processed.
 * @returns {string} The processed string with non-Latin characters removed, multiple spaces replaced by a single space, and trimmed.
 */
export function trimNonLatin(data: string) {
    return data.replace(/[^\x00-\x7F]/g, "")
        .replace(/ +/g, ' ')
        .trim()
}

/**
 * A class that provides a blank writer implementation.
 * 
 * This class is used to provide a no-op implementation of a writer, making it compatible with other writer interfaces.
 */
export class BlankWriter {
    constructor() {
    }

    /**
     * Initializes the writer.
     * 
     * This method does nothing and is provided for compatibility with other writer interfaces.
     */
    async init() {
        //do nothing, just to make compatible with other writer
    }

    /**
     * Writes data to the writer.
     * 
     * This method does nothing and is provided for compatibility with other writer interfaces.
     * 
     * @param {string} key - The key associated with the data.
     * @param {Uint8Array|string} data - The data to be written.
     */
    async write(key: string, data: Uint8Array | string) {
        //do nothing, just to make compatible with other writer
    }

    /**
     * Ends the writing process.
     * 
     * This method does nothing and is provided for compatibility with other writer interfaces.
     */
    async end() {
        //do nothing, just to make compatible with other writer
    }
}

/**
 * A debugging class for performance measurement.
*/

export class PerformanceDebugger {
    kv: { [key: string]: number[] } = {}
    startTime: number
    endTime: number

    /**
     * Starts the timing measurement.
    */
    start() {
        this.startTime = performance.now()
    }

    /**
     * Ends the timing measurement and records the time difference.
     * 
     * @param {string} key - The key to associate with the recorded time.
    */
    endAndRecord(key: string) {
        this.endTime = performance.now()
        if (!this.kv[key]) {
            this.kv[key] = []
        }
        this.kv[key].push(this.endTime - this.startTime)
    }

    /**
     * Ends the timing measurement, records the time difference, and starts a new timing measurement.
     * 
     * @param {string} key - The key to associate with the recorded time.
    */
    endAndRecordAndStart(key: string) {
        this.endAndRecord(key)
        this.start()
    }

    /**
     * Logs the average time for each key to the console.
    */
    log() {
        let table: { [key: string]: number } = {}

        for (const key in this.kv) {
            table[key] = this.kv[key].reduce((a, b) => a + b, 0) / this.kv[key].length
        }


        console.table(table)
    }

    combine(other: PerformanceDebugger) {
        for (const key in other.kv) {
            if (!this.kv[key]) {
                this.kv[key] = []
            }
            this.kv[key].push(...other.kv[key])
        }
    }
}

export function getLanguageCodes() {
    let languageCodes: {
        code: string
        name: string
    }[] = []

    for (let i = 0x41; i <= 0x5A; i++) {
        for (let j = 0x41; j <= 0x5A; j++) {
            languageCodes.push({
                code: String.fromCharCode(i) + String.fromCharCode(j),
                name: ''
            })
        }
    }

    languageCodes = languageCodes.map(v => {
        return {
            code: v.code.toLocaleLowerCase(),
            name: new Intl.DisplayNames([
                DBState.db.language === 'cn' ? 'zh' : DBState.db.language
            ], {
                type: 'language',
                fallback: 'none'
            }).of(v.code)
        }
    }).filter((a) => {
        return a.name
    }).sort((a, b) => a.name.localeCompare(b.name))

    return languageCodes
}

export function getVersionString(): string {
    let versionString = versionData.version
    if(appSubVer) {
        versionString += '-' + appSubVer
    }
    if (import.meta.env.VITE_RISU_NIGHTLY_BUILD === 'TRUE') {
        versionString = 'Nightly Build ' + import.meta.env.VITE_RISU_BUILD_TIME
    }
    if (window.location.hostname === 'stable.risuai.xyz') {
        versionString += ' (Stable)';
    }
    return versionString
}

export function toGetter<T extends object>(
    getterFn: () => T,
    args?: {
        //blocks this.children from being accessed
        restrictChildren:string[]
    }
): T {

    const dummyTarget = () => { };

    return new Proxy(dummyTarget, {
        get(target, prop, receiver) {

            const realInstance = getterFn();
            
            if (args?.restrictChildren && args.restrictChildren.includes(prop as string)) {
                throw new Error(`Access to property '${String(prop)}' is restricted`);
            }

            if (realInstance === null || realInstance === undefined) {
                return (realInstance as any)[prop];
            }

            const value = Reflect.get(realInstance as object, prop);

            if (typeof value === 'function') {
                return value.bind(realInstance);
            }

            return value;
        },

        set(target, prop, value, receiver) {

            if(args?.restrictChildren && args.restrictChildren.includes(prop as string)) {
                throw new Error(`Access to property '${String(prop)}' is restricted`);
            }
            const realInstance = getterFn();
            return Reflect.set(realInstance as object, prop, value, receiver);
        },

        has(target, prop) {
            const realInstance = getterFn();
            return Reflect.has(realInstance as object, prop);
        },

        ownKeys(target) {
            const realInstance = getterFn();
            return Reflect.ownKeys(realInstance as object);
        },

        construct(target, argArray, newTarget) {
            const realInstance = getterFn() as any;
            return new realInstance(...argArray);
        },

        deleteProperty(target, prop) {
            const realInstance = getterFn();
            return Reflect.deleteProperty(realInstance as object, prop);
        },

        getPrototypeOf() {
            const realInstance = getterFn();
            return Reflect.getPrototypeOf(realInstance as object);
        }
    }) as unknown as T;
}

const countriesWithAiLaw = new Set<string>([

    // EU
    // AI Act
    // https://artificialintelligenceact.eu/
    
    "AT",
    "BE",
    "BG",
    "HR",
    "CY",
    "CZ",
    "DK",
    "EE",
    "FI",
    "FR",
    "DE",
    "EL",
    "GR",
    "HU",
    "IE",
    "IT",
    "LV",
    "LT",
    "LU",
    "MT",
    "NL",
    "PL",
    "PT",
    "RO",
    "SK",
    "SI",
    "ES",
    "SE",

    //China 
    //Measures for Labeling of AI-Generated Synthetic Content
    // 关于印发《人工智能生成合成内容标识办法》的通知 
    // https://www.cac.gov.cn/2025-03/14/c_1743654684782215.htm
    "CN",

    //Although CN Law doesn't apply, just in case
    "HK",
    "MO",

    //TW isn't under mainland china jurisdiction
    //de facto, de jure in TW law, unlike HK and MO,
    //So we don't include it for now
    //"TW", 

    // Republic of Korea
    // AI Basic Act
    // 인공지능 발전과 신뢰 기반 조성 등에 관한 기본법
    // https://www.law.go.kr/%EB%B2%95%EB%A0%B9/%EC%9D%B8%EA%B3%B5%EC%A7%80%EB%8A%A5%20%EB%B0%9C%EC%A0%84%EA%B3%BC%20%EC%8B%A0%EB%A2%B0%20%EA%B8%B0%EB%B0%98%20%EC%A1%B0%EC%84%B1%20%EB%93%B1%EC%97%90%20%EA%B4%80%ED%95%9C%20%EA%B8%B0%EB%B3%B8%EB%B2%95/(20676,20250121)
    "KR",

    // Vietnam
    // Digital Tech Law
    // Luật Công nghệ số
    "VN",

])

export function aiLawApplies(): boolean {

    //TODO: implement actual logic
    //lets now assume it always applies
    //so we don't have legal issues later

    return true
}

export function aiWatermarkingLawApplies(): boolean {

    //TODO: implement actual logic
    //lets now assume it is false for now,
    //becuase very few countries have it for now
    return false
}

const foldTargetContext = {
    captureCurrent: () => {
        const character = DBState.db.characters[selIdState.selId]
        const conversation = character?.chats[character.chatPage]
        return character && conversation ? { character, conversation } : null
    },
    getCurrentSession: () => persistentDataRuntime.getActiveConversationSession(),
    captureSelectedConversationTarget: () =>
        persistentDataRuntime.captureSelectedConversationTarget(),
    acquirePersistentRevision: (revision: number) =>
        persistentDataRuntime.getPersistentDataRuntime().store.acquireRevision(revision),
    acquireCompleteConversation: (reason: string, target?: Parameters<
        typeof persistentDataRuntime.acquireCompleteConversation
    >[1]) => persistentDataRuntime.acquireCompleteConversation(reason, target),
}

export const chatFoldedState = $state<{
    data: null | CapturedChatMessageTarget
}>({
    data: null
})

//Since its exported, we cannot use $derived here
export let chatFoldedStateMessageIndex = $state({
    index: -1
})

$effect.root(() => {
    $effect(() => {
        if(chatFoldedState.data === null){
            chatFoldedStateMessageIndex.index = -1
            return
        }
        const target = resolveRetainedChatMessageTarget(chatFoldedState, foldTargetContext)
        if(!target){
            console.warn('Target message for folding is stale')
            chatFoldedStateMessageIndex.index = -1
            return
        }
        chatFoldedStateMessageIndex.index = target.absoluteIndex
    })
})

let foldQueryGeneration = 0

export async function foldChatToMessage(targetMessageIdOrIndex: string | number) {
    const generation = ++foldQueryGeneration
    const target = typeof targetMessageIdOrIndex === 'number'
        ? await queryChatMessageTargetAt(foldTargetContext, targetMessageIdOrIndex)
        : await queryChatMessageTargetById(foldTargetContext, targetMessageIdOrIndex, 'first')
    if (generation !== foldQueryGeneration) return false
    chatFoldedState.data = target
    return target !== null
}

export async function changeChatTo(IdOrIndex: string | number): Promise<boolean> {
    const characterId = DBState.db.characters[selIdState.selId]?.chaId
    if(!characterId) return false
    const character = DBState.db.characters.find((value) => value.chaId === characterId)
    const chats = Array.isArray(character?.chats) ? character.chats : []
    const chatId = typeof IdOrIndex === 'number'
        ? chats[IdOrIndex]?.id
        : IdOrIndex
    if(!chatId || !chats.some((chat) => chat.id === chatId)) return false

    fencePersistentNavigation()
    const activity = beginNavigationActivity('conversation')
    try {
        await yieldToUi()
        if (!activity.isCurrent()) return false
        const currentCharacter = DBState.db.characters[selIdState.selId]
        if (
            currentCharacter?.chaId !== characterId ||
            !Array.isArray(currentCharacter.chats) ||
            !currentCharacter.chats.some((chat) => chat.id === chatId)
        )
            return false
        const activated = await activateConversation(chatId)
        return activity.isCurrent() ? activated : false
    } finally {
        activity.finish()
    }
}

export function createChatCopyName(originalName: string,type:'Copy'|'Branch'): string {
    let name = originalName.replaceAll(/\(((Copy|Branch)( \d+)?)\)$/g, '').trim()
    let copyIndex = 1
    let newName = `${name} (${type})`
    const char = getCurrentCharacter()
    while (char.chats.find((v) => v.name === newName)) {
        copyIndex++
        newName = `${name} (${type} ${copyIndex})`
    }
    return newName
}
