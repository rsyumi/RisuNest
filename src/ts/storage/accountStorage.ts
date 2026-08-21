import { writable } from "svelte/store"
import { getDatabase } from "./database.svelte"
import localforage from "localforage"
import { alertLogin, alertNormalWait, alertStore } from "../alert"
import { forageStorage, getUncleanables, getUncleanablesSync } from "../globalApi.svelte"
import { encodeRisuSaveLegacy } from "./risuSave"
import { v4 } from "uuid"
import { language } from "src/lang"
import { fetchProtectedResource } from "../sionyw"
import { completeAccountUnmigration } from "./databaseRestore"
import { replacePersistentDatabase } from "./persistentDataRuntime.svelte"

export const AccountWarning = writable('')
let risuSession = ''
const cachedForage = localforage.createInstance({name: "risuaiAccountCached"})

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

async function cacheDatabaseWrite(key:string, value:Uint8Array, saveDate:string):Promise<void> {
    if(key !== accountDatabaseKey){
        return
    }
    await cachedForage.setItem(key, value)
    await cachedForage.setItem(key + '__date', saveDate)
}

export class AccountStorage{
    auth:string
    usingSync:boolean

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
                await cacheDatabaseWrite(key, value, saveDate)
                return { kind: 'not-modified', replacementKey: key }
            }
            if(da.status === 403){
                await discardResponseBody(da)
                if(da.headers.get('x-risu-status') === 'warn'){
                    return { kind: 'auth-warning' }
                }
                localStorage.setItem("fallbackRisuToken",await alertLogin())
                this.checkAuth()
                continue
            }
            if(da.status < 200 || da.status >= 300){
                throw await getDaText()
            }

            if(isJsonResponse(da)){
                const json = JSON.parse(await getDaText())
                if(json?.warning){
                    if(!seenWarnings.includes(json.warning)){
                        seenWarnings.push(json.warning)
                        AccountWarning.set(json.warning)
                    }
                }
                if(json?.reloadSession){
                    void alertNormalWait(language.activeTabChange).then(() => {
                        location.reload()
                    })
                    await waitForever()
                }
            }

            const replacementKey = await getDaText()
            if(key.startsWith('assets/')){
                await localforage.setItem(key, new Uint8Array(value).buffer)
            }
            await cacheDatabaseWrite(key, value, saveDate)
            return { kind: 'written', replacementKey }
        }

        throw new Error('Account write did not complete')
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
            const cached:ArrayBuffer|null = await localforage.getItem(key)
            if(cached){
                return { kind: 'value', bytes: new Uint8Array(cached) }
            }
        }
        let da:Response|undefined
        const saveDate = await cachedForage.getItem(key + '__date') as number|string|undefined
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
                localStorage.setItem("fallbackRisuToken",await alertLogin())
                this.checkAuth()
            }
        }
        if(da.status === 303){
            const data = await da.json()
            if(data.match){
                const cached = await cachedForage.getItem(key) as ArrayBuffer|Uint8Array|null
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
            await localforage.setItem(key, ab)
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


    listItem = this.keys
}

export async function unMigrationAccount() {
    const keys = await forageStorage.keys()
    const db = getDatabase()
    let i = 0;
    const MigrationStorage = localforage.createInstance({name: "risuai"})
    
    for(const key of keys){
        if(key === 'database/database.bin'){
            continue
        }
        alertStore.set({
            type: "wait",
            msg: `Migrating your data...(${i}/${keys.length})`
        })
        await MigrationStorage.setItem(key,await forageStorage.getItem(key))
        i += 1
    }

    await completeAccountUnmigration(db, {
        replaceDatabase: replacePersistentDatabase,
        captureAcceptedDatabase: getDatabase,
        writeLegacyMirror: async (database) => {
            await MigrationStorage.setItem('database/database.bin', encodeRisuSaveLegacy(database))
        },
        finalize: () => {
            alertStore.set({ type: "none", msg: "" })
            localStorage.setItem('dosync', 'avoid')
            localStorage.removeItem('accountst')
            localStorage.removeItem('fallbackRisuToken')
            location.reload()
        },
    })
}
