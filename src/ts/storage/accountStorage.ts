import { writable } from "svelte/store"
import { getDatabase } from "./database.svelte"
import localforage from "localforage"
import { alertLogin, alertNormalWait, alertStore } from "../alert"
import { getUncleanablesSync } from "../globalApi.svelte"
import { v4 } from "uuid"
import { language } from "src/lang"
import { fetchProtectedResource } from "../sionyw"
import { completeAccountUnmigration } from "./databaseRestore"
import {
    materializePersistentDatabaseSnapshotWithRevision,
    replacePersistentDatabase,
} from "./persistentDataRuntime.svelte"
import { isTauri } from "../platform"

export const AccountWarning = writable('')
let risuSession = ''

export function resetAccountStorageSession(): void {
    risuSession = ''
}

let seenWarnings:string[] = []
const accountDatabaseKey = 'database/database.bin'

export type AccountReadResult =
    | { kind: 'value'; bytes: Uint8Array }
    | { kind: 'not-modified'; bytes: Uint8Array }
    | { kind: 'missing' }

export type AccountWriteResult =
    | { kind: 'written'; replacementKey: string }
    | { kind: 'not-modified'; replacementKey: string }
    | { kind: 'auth-warning' }

export interface AccountReadOptions {
    progress?(ratio: number): void
    signal?: AbortSignal
}

export interface AccountWriteOptions {
    signal?: AbortSignal
}

export interface AccountNativeOfficialWriteAttemptContext {
    credential: { kind: 'risu-auth'; token: string }
    session: string | null
    saveDate: string
    signal?: AbortSignal
}

export type AccountNativeOfficialWriteAttemptResult<T> =
    | {
        kind: 'written' | 'not-modified'
        session: string | null
        replacementKey: string
        warning?: string | null
        reloadSession?: boolean
        receipt: T
    }
    | { kind: 'auth-warning'; session: string | null }
    | { kind: 'reauthentication-needed'; session: string | null }

export type AccountNativeOfficialWriteResult<T> =
    | {
        kind: 'written' | 'not-modified'
        replacementKey: string
        receipt: T
        completeReload(): Promise<void>
    }
    | { kind: 'auth-warning' }

export type AccountNativeOfficialWriteAttempt<T> = (
    context: AccountNativeOfficialWriteAttemptContext,
) => Promise<AccountNativeOfficialWriteAttemptResult<T>>

export interface AccountStorageCache {
    getItem(key: string): Promise<unknown | null>
    setItem(key: string, value: unknown): Promise<unknown>
}

export interface AccountCredentialRouting {
    getToken(): string | null | undefined
    reauthenticate(loginResult: string): Promise<void>
}

export interface AccountStorageOptions {
    databaseCache?: AccountStorageCache
    assetCache?: AccountStorageCache
    credentialRouting?: AccountCredentialRouting
}

function withSignal(options: RequestInit, signal?: AbortSignal): RequestInit {
    return signal ? { ...options, signal } : options
}

function isJsonResponse(response: Response): boolean {
    return /^\s*application\/json\s*(?:;|$)/i.test(response.headers.get('content-type') ?? '')
}

async function discardResponseBody(response: Response): Promise<void> {
    try {
        await response.body?.cancel()
    } catch (error) {}
}

function waitForever(): Promise<never> {
    return new Promise(() => {})
}

function publishAccountWarning(warning: string | null | undefined): void {
    if (!warning || seenWarnings.includes(warning)) return
    seenWarnings.push(warning)
    AccountWarning.set(warning)
}

async function applyAccountReload(reloadSession: boolean): Promise<void> {
    if (!reloadSession) return
    void alertNormalWait(language.activeTabChange).then(() => {
        location.reload()
    })
    await waitForever()
}

async function cacheDatabaseWrite(
    cache: AccountStorageCache,
    key:string,
    value:Uint8Array,
    saveDate:string,
):Promise<void> {
    if(key !== accountDatabaseKey){
        return
    }
    await cache.setItem(key, value)
    await cache.setItem(key + '__date', saveDate)
}

export class AccountStorage{
    auth:string
    usingSync:boolean
    private readonly databaseCache: AccountStorageCache
    private readonly assetCache: AccountStorageCache
    private readonly credentialRouting?: AccountCredentialRouting

    constructor(options: AccountStorageOptions = {}) {
        this.databaseCache = options.databaseCache
            ?? localforage.createInstance({ name: 'risuaiAccountCached' })
        this.assetCache = options.assetCache ?? localforage
        this.credentialRouting = options.credentialRouting
    }

    async setItem(key:string, value:Uint8Array) {
        const result = await this.writeItem(key, value)
        if(result.kind === 'auth-warning'){
            return undefined
        }
        return result.replacementKey
    }

    async writeItem(
        key:string,
        value:Uint8Array,
        options:AccountWriteOptions = {},
    ):Promise<AccountWriteResult> {
        this.checkAuth()
        let da:Response|undefined

        while((!da) || da.status === 403){

            const saveDate = Date.now().toFixed(0)

            if(risuSession === ''){
                da = await fetchProtectedResource('/api/account/getsessionnumber', withSignal({
                    method: "GET"
                }, options.signal))

                const json = await da.json()
                risuSession = `${json.sessionNumber}`
            }

            da = await fetchProtectedResource('/api/account/write', withSignal({
                method: "POST",
                body: value as any,
                headers: {
                    'content-type': 'application/octet-stream',
                    'x-risu-key': key,
                    'X-Format': 'nocheck',
                    'x-risu-session': risuSession,
                    'x-risu-save-date': saveDate
                }
            }, options.signal))
            let daText:string|undefined = undefined
            const getDaText = async () => {
                if(daText === undefined){
                    daText = await da!.text()
                }
                return daText
            }

            if(da.status === 304){
                await discardResponseBody(da)
                await cacheDatabaseWrite(this.databaseCache, key, value, saveDate)
                return { kind: 'not-modified', replacementKey: key }
            }
            if(da.status === 403){
                await discardResponseBody(da)
                if(da.headers.get('x-risu-status') === 'warn'){
                    return { kind: 'auth-warning' }
                }
                await this.reauthenticate()
                continue
            }
            if(da.status < 200 || da.status >= 300){
                throw await getDaText()
            }

            if(isJsonResponse(da)){
                const json = JSON.parse(await getDaText())
                publishAccountWarning(json?.warning)
                await applyAccountReload(Boolean(json?.reloadSession))
            }

            const replacementKey = await getDaText()
            if(key.startsWith('assets/')){
                await this.assetCache.setItem(key, new Uint8Array(value).buffer)
            }
            await cacheDatabaseWrite(this.databaseCache, key, value, saveDate)
            return { kind: 'written', replacementKey }
        }

        throw new Error('Account write did not complete')
    }

    async writeOfficialDatabaseFromNative<T>(
        attempt: AccountNativeOfficialWriteAttempt<T>,
        options: AccountWriteOptions = {},
    ): Promise<AccountNativeOfficialWriteResult<T> | null> {
        while (true) {
            this.checkAuth()
            if (localStorage.getItem('ignoreRisuAuth') === 'true' || !this.auth) return null

            const result = await attempt({
                credential: { kind: 'risu-auth', token: this.auth },
                session: risuSession || null,
                saveDate: Date.now().toFixed(0),
                signal: options.signal,
            })
            if (result.session !== null) risuSession = result.session
            if (result.kind === 'reauthentication-needed') {
                await this.reauthenticate()
                continue
            }
            if (result.kind === 'auth-warning') return { kind: 'auth-warning' }
            publishAccountWarning(result.warning)
            return {
                kind: result.kind,
                replacementKey: result.replacementKey,
                receipt: result.receipt,
                completeReload: () => applyAccountReload(result.reloadSession ?? false),
            }
        }
    }

    async getItem(key:string, callback?:(status:number) => void):Promise<Buffer|null> {
        const result = await this.readItem(key, { progress: callback })
        if(result.kind === 'missing'){
            return null
        }
        return Buffer.from(result.bytes)
    }

    async readItem(
        key:string,
        options:AccountReadOptions = {},
    ):Promise<AccountReadResult> {
        this.checkAuth()
        if(key.startsWith('assets/')){
            const cached = await this.assetCache.getItem(key)
            if(cached instanceof ArrayBuffer || ArrayBuffer.isView(cached)){
                return { kind: 'value', bytes: new Uint8Array(
                    cached instanceof ArrayBuffer
                        ? cached
                        : cached.buffer.slice(cached.byteOffset, cached.byteOffset + cached.byteLength),
                ) }
            }
        }
        let da:Response|undefined
        const saveDate = await this.databaseCache.getItem(key + '__date') as number|string|undefined
        while((!da) || da.status === 403){
            da = await fetchProtectedResource('/api/account/read/' + Buffer.from(key ,'utf-8').toString('hex') +
                (key === accountDatabaseKey ? ('|' + v4()) : ''), withSignal({
                method: "GET",
                headers: {
                    'x-risu-key': key,
                    'x-risu-save-date': (saveDate || 0).toString()
                }
            }, options.signal))
            if(da.status === 403){
                await discardResponseBody(da)
                await this.reauthenticate()
            }
        }
        if(da.status === 303){
            const data = await da.json()
            if(data.match){
                const cached = await this.databaseCache.getItem(key) as ArrayBuffer|Uint8Array|null
                if(!cached){
                    throw new Error(`Cached account bytes are missing for ${key}`)
                }
                return { kind: 'not-modified', bytes: new Uint8Array(cached) }
            }
            else{
                return { kind: 'missing' }
            }
        }

        if(da.status < 200 || da.status >= 300){
            throw await da.text()
        }
        if(da.status === 204){
            return { kind: 'missing' }
        }
        if(key.startsWith('assets/')){
            const ab = await da.arrayBuffer()
            await this.assetCache.setItem(key, ab)
            return { kind: 'value', bytes: new Uint8Array(ab) }
        }
        if(!options.progress){
            const ab = await da.arrayBuffer()
            return { kind: 'value', bytes: new Uint8Array(ab) }
        }
        const size = parseInt(da.headers.get('x-body-size'))
        const appendable = new Uint8Array(size)
        const reader = da.body.getReader()

        let i = 0
        while(true){
            const {done, value} = await reader.read()
            if(done){
                break
            }
            appendable.set(value, i)
            i += value.length
            options.progress(i/size)
        }

        return { kind: 'value', bytes: appendable }
    }
    keys():string[]{
        let db = getDatabase()
        return getUncleanablesSync(db, 'pure')
    }
    removeItem(key:string){
        throw "Error: You cannot remove data in account. report this to dev if you found this."
    }

    private checkAuth(){
        if (this.credentialRouting) {
            this.auth = this.credentialRouting.getToken() ?? ''
            return
        }
        const db = getDatabase()
        this.auth = db?.account?.token
        if(!this.auth){
            try {
                db.account = JSON.parse(localStorage.getItem("fallbackRisuToken"))
                this.auth = db?.account?.token
                db.account.useSync = true
            } catch (error) {}
        }
    }

    private async reauthenticate(): Promise<void> {
        const loginResult = await alertLogin()
        if (this.credentialRouting) {
            await this.credentialRouting.reauthenticate(loginResult)
        } else {
            localStorage.setItem("fallbackRisuToken", loginResult)
        }
        this.checkAuth()
    }


    listItem = this.keys
}

export async function unMigrationAccount() {
    if (isTauri) {
        throw new Error('Account unmigration is only available on the web')
    }
    const snapshot = await materializePersistentDatabaseSnapshotWithRevision(
        'account-unmigration',
    )
    const db = snapshot.database
    const expectedRevision = snapshot.revision
    const expectedMutationGeneration = snapshot.mutationGeneration
    const { materializeAccountUnmigrationResources } = await import("./databaseRestore")
    const { resolveBlobStore } = await import("./platformBlobStore")
    const { selectLegacyBackupAssetKeys } = await import("../drive/backupAssets")
    const { storeActiveAsset } = await import("./accountAssetAccess")
    const {
        getAccountColdStorageItem,
        getColdStorageItem,
        isColdStorageBackupData,
        listColdDataKeys,
        setLocalColdStorageItem,
    } = await import("../process/coldstorage.svelte")
    const blobStore = await resolveBlobStore()
    const accountStorage = new AccountStorage()
    const coldKeys = await listColdDataKeys(db)

    await completeAccountUnmigration(db, {
        prepareResources: () => materializeAccountUnmigrationResources({
            coldKeys,
            collectAssetKeys: (selectedCold) => {
                const chars = db.characters.map((character) => {
                    if (!character.coldstorage) return character
                    const selected = selectedCold.get(character.coldstorage) as {
                        character?: typeof character
                    } | undefined
                    return selected?.character?.chaId === character.chaId
                        ? selected.character
                        : character
                })
                return selectLegacyBackupAssetKeys(
                    getUncleanablesSync(db, 'pure', { chars }),
                )
            },
            isValidCold: isColdStorageBackupData,
            readLocalAsset: (key) => blobStore.read(key),
            readRemoteAsset: async (key) => {
                const result = await accountStorage.readItem(key)
                return result.kind === 'missing' ? null : result.bytes
            },
            writeLocalAsset: async (key, bytes) => {
                const name = key.replace(/\\/g, '/').split('/').pop() ?? key
                await storeActiveAsset(blobStore, key, bytes, {
                    kind: 'asset',
                    mime: '',
                    name,
                    ext: name.split('.').pop() ?? '',
                })
            },
            readLocalCold: (key) => getColdStorageItem(key, { accountFallback: true }),
            readRemoteCold: async (key) => {
                const value = await getAccountColdStorageItem(key)
                if (value !== null && !isColdStorageBackupData(value)) {
                    throw new Error(`Invalid account cold payload: ${key}`)
                }
                return value
            },
            writeLocalCold: async (key, value) => {
                if (!await setLocalColdStorageItem(key, value)) {
                    throw new Error(`Failed to write local cold payload: ${key}`)
                }
            },
        }),
        replaceDatabase: (database, reason) => replacePersistentDatabase(database, reason, {
            authoritative: true,
            expectedRevision,
            expectedMutationGeneration,
        }),
        finalize: () => {
            alertStore.set({ type: "none", msg: "" })
            localStorage.setItem('dosync', 'avoid')
            localStorage.removeItem('accountst')
            localStorage.removeItem('fallbackRisuToken')
            location.reload()
        },
    })
}
