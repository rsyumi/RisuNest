import { forageStorage } from "../globalApi.svelte"
import { isNodeServer } from "src/ts/platform"
import { DBState, selectedCharID } from "../stores.svelte"
import { compress as fflateCompress, decompress as fflateDecompress } from "fflate"
import { fetchProtectedResource } from "../sionyw"
import { alertClear, alertConfirm, alertError, alertWait } from "../alert"
import { language } from "src/lang"
import type { Database } from "../storage/database.svelte"
import { coldStorageHeader, getColdStorageAffectedCharacters, getColdStorageBackupName, isColdStorageBackupData, listColdDataKeysFromDb } from "./coldstorageData"
import { compactColdStorageDatabase } from "../storage/coldStorageCompaction"
import {
    getActiveConversationSession,
    getPersistentDataRuntime,
    getPersistentNavigationGeneration,
} from "../storage/persistentDataRuntime.svelte"
import type { LocalColdStorageRuntime } from "../storage/localColdStorageRuntime"
import { hasIncompletePersistentWorkingSet } from '../storage/workingSetCatalog'
import { workingSetResidency } from '../storage/workingSetResidency'
import { get } from 'svelte/store'

export {
    coldStorageHeader,
    getColdStorageBackupKey,
    getColdStorageBackupName,
    isColdStorageBackupData,
    replaceColdStoragePayloadResources,
    listColdDataKeysFromDb
} from "./coldstorageData"

let localColdStorageRuntime: LocalColdStorageRuntime | null = null

export function configureLocalColdStorageRuntime(runtime: LocalColdStorageRuntime): void {
    localColdStorageRuntime = runtime
}

function requireLocalColdStorageRuntime(): LocalColdStorageRuntime {
    if(!localColdStorageRuntime){
        throw new Error('Local cold storage runtime is not configured')
    }
    return localColdStorageRuntime
}

async function decompress(data:Uint8Array) {
    return new Promise<Uint8Array>((resolve, reject) => {
        fflateDecompress(data, (err, decompressed) => {
            if (err) {
                return reject(err)
            }
            resolve(decompressed)
        })
    })
}

export async function getColdStorageItem(key:string, opts:{
    accountFallback?:boolean
} = {}):Promise<any> {

    if(forageStorage.isAccount && !opts.accountFallback){
        const value = await getAccountColdStorageItem(key)
        if(value !== null){
            return value
        }
        return await getColdStorageItem(key, {
            accountFallback: true
        })
    }
    return requireLocalColdStorageRuntime().read(key)
}

async function discardResponseBody(response:Response):Promise<void> {
    try {
        await response.body?.cancel()
    } catch (error) {}
}

function isAbortError(error:unknown):boolean {
    return typeof error === 'object' && error !== null && 'name' in error && error.name === 'AbortError'
}

export async function getAccountColdStorageItem(
    key:string,
    signal?:AbortSignal,
):Promise<unknown|null> {
    const d = await fetchProtectedResource('/hub/account/coldstorage', {
        method: 'GET',
        headers: {
            'x-risu-key': key,
        },
        ...(signal ? { signal } : {}),
    })

    if(d.status !== 200){
        await discardResponseBody(d)
        return null
    }
    const buf = await d.arrayBuffer()
    const text = new TextDecoder().decode(await decompress(new Uint8Array(buf)))
    return JSON.parse(text)
}

async function compressColdStorageValue(value:any):Promise<Uint8Array | null> {
    try {
        const json = JSON.stringify(value)
        return await (new Promise<Uint8Array>((resolve, reject) => {
            fflateCompress(new TextEncoder().encode(json), (err, result) => {
                if (err) {
                    return reject(err)
                }
                resolve(result)
            })
        }))
    } catch (error) {
        console.error('Cold storage compression failed:', error)
        return null
    }
}

export async function setAccountColdStorageItem(
    key:string,
    value:any,
    signal?:AbortSignal,
):Promise<boolean> {
    const compressed = await compressColdStorageValue(value)
    if(!compressed){
        return false
    }

    try {
        const res = await fetchProtectedResource('/hub/account/coldstorage', {
            method: 'POST',
            headers: {
                'x-risu-key': key,
                'content-type': 'application/octet-stream'
            },
            body: compressed as any,
            ...(signal ? { signal } : {}),
        })
        if(res.status !== 200){
            console.error('Error setting cold storage item:', await res.text().catch(() => 'unknown'))
            return false
        }
        await discardResponseBody(res)
        return true
    } catch (error) {
        if(isAbortError(error)){
            throw error
        }
        console.error('Cold storage account write failed:', error)
        return false
    }
}

export async function setColdStorageItem(key:string, value:any):Promise<boolean> {
    console.log("setting cold storage item", key, value)

    if(forageStorage.isAccount){
        return await setAccountColdStorageItem(key, value)
    }

    return setLocalColdStorageItem(key, value)
}

export async function setLocalColdStorageItem(key:string, value:any):Promise<boolean> {
    return requireLocalColdStorageRuntime().write(key, value)
}

export async function listColdStorageItems():Promise<{items:string[]}> {
    if(forageStorage.isAccount){
        const d = await fetchProtectedResource('/hub/account/coldstorage', {
            method: 'GET',
            headers: {
                'x-risu-key': '@list-keys',
            }
        })

        if(d.status === 200){
            return await d.json()
        }
        return null
    }

    return { items: await requireLocalColdStorageRuntime().list() }
}

export async function cleanColdStorage(){
    if (hasIncompletePersistentWorkingSet(DBState.db, workingSetResidency)) return
    const actualUsedKeys = await listColdDataKeys()
    const allKeys = (await listColdStorageItems()).items
    const unusedKeys = allKeys.filter(k => !actualUsedKeys.includes(k))
    console.log('Cleaning cold storage, actual used keys:', actualUsedKeys, 'all keys:', allKeys, 'unused keys:', unusedKeys)

    if(forageStorage.isAccount || isNodeServer){
        await removeColdStorageItems(unusedKeys)
    }
    else{
        for(let i=0;i<unusedKeys.length;i++){
            const key = unusedKeys[i]
            alertWait(`Removing unused cold storage item: ${key} (${i + 1} / ${unusedKeys.length})`)
            await removeColdStorageItems([key])
        }
    }

    alertClear()
}

async function removeColdStorageItems(keys:string[]) {
    
    if(forageStorage.isAccount){
        try {
            const res = await fetchProtectedResource('/hub/account/coldstorage', {
                method: 'POST',
                headers: {
                    'x-risu-key': 'remove',
                    'x-action': 'remove'
                },
                body: JSON.stringify({ keys })
            })
            if(res.status !== 200){
                console.error('Error removing cold storage item:', await res.text().catch(() => 'unknown'))
            }
        } catch (error) {
            console.error('Cold storage account remove failed:', error)
        }
    }
    else{
        try {
            await requireLocalColdStorageRuntime().remove(keys)
        } catch (error) {
            console.error('Cold storage remove failed:', error)
        }
    }
}

export async function listColdDataKeys(db: Pick<Database, 'characters'> = DBState.db): Promise<string[]> {
    return listColdDataKeysFromDb(db)
}

export type ColdStorageBackupPayload = {
    key: string
    backupName: string
    value: unknown
}

export async function collectColdStorageBackupPayloads(db: Pick<Database, 'characters'> = DBState.db): Promise<{
    payloads: ColdStorageBackupPayload[]
    missingKeys: string[]
    invalidKeys: string[]
}> {
    const coldKeys = await listColdDataKeys(db)
    const payloads: ColdStorageBackupPayload[] = []
    const missingKeys: string[] = []
    const invalidKeys: string[] = []

    for (const key of coldKeys) {
        try {
            const value = await getColdStorageItem(key)
            if (!value) {
                missingKeys.push(key)
                continue
            }

            if (!isColdStorageBackupData(value)) {
                invalidKeys.push(key)
                continue
            }

            payloads.push({
                key,
                backupName: getColdStorageBackupName(key),
                value,
            })
        } catch (error) {
            console.error(`Failed to read cold storage item ${key}:`, error)
            missingKeys.push(key)
        }
    }

    return { payloads, missingKeys, invalidKeys }
}

export async function confirmIncompleteColdStorageOperation(
    db: Pick<Database, 'characters'>,
    unavailableKeys: Iterable<string>,
    operation: 'backup' | 'restore',
): Promise<boolean> {
    const uniqueUnavailableKeys = Array.from(new Set(unavailableKeys))
    if (uniqueUnavailableKeys.length === 0) {
        return true
    }

    const affected = getColdStorageAffectedCharacters(db, uniqueUnavailableKeys)
    const characterNames = affected.characterNames.join(', ')
    const message = operation === 'backup'
        ? language.errors.coldStorageIncompleteBackupConfirm(
            characterNames,
            uniqueUnavailableKeys.length,
            affected.unresolvedKeys.length,
        )
        : language.errors.coldStorageIncompleteRestoreConfirm(
            characterNames,
            uniqueUnavailableKeys.length,
            affected.unresolvedKeys.length,
        )

    return await alertConfirm(message)
}

export async function makeColdData():Promise<boolean>{
    try {
        if (
            !DBState.db.coldstorage
            || hasIncompletePersistentWorkingSet(DBState.db, workingSetResidency)
        ) return false
        const runtime = getPersistentDataRuntime()
        const token = await runtime.capturePersistentMutationToken('cold-storage-compaction')
        const database = DBState.db
        const authorityEpoch = runtime.getStorageAuthorityEpoch()
        return await compactColdStorageDatabase(database, {
            now: Date.now(),
            createId: () => crypto.randomUUID(),
            write: setLocalColdStorageItem,
            read: (key) => getColdStorageItem(key, { accountFallback: true }),
            replaceDatabase: (candidate, reason) => {
                if (
                    DBState.db !== database
                    || runtime.getStorageAuthorityEpoch() !== authorityEpoch
                ) {
                    throw new Error('Database changed during cold storage compaction')
                }
                return runtime.replacePersistentDatabase(candidate, reason, {
                    expectedMutationGeneration: token.mutationGeneration,
                    expectedRevision: runtime.revision,
                })
            },
            onProgress: (phase, remaining) => {
                const target = phase === 'character' ? 'character' : 'chat'
                alertWait(`Creating ${target} cold storage data... ${remaining} items left`)
            },
            onFailure: (failure) => {
                const action = failure.kind === 'write' ? 'write' : failure.kind
                if(failure.target === 'character'){
                    const character = DBState.db.characters[failure.characterIndex]
                    console.error(`Cold storage ${action} failed for character ${character?.chaId ?? failure.characterIndex}, keeping original data`)
                    return
                }
                const chat = DBState.db.characters[failure.characterIndex]?.chats[failure.chatIndex ?? -1]
                console.error(`Cold storage ${action} failed for chat ${chat?.id ?? failure.chatIndex}, keeping original data`)
                alertError(failure.kind === 'write'
                    ? language.errors.coldStorageWriteFailed
                    : language.errors.coldStorageVerifyFailed)
            },
        })
    } finally {
        alertClear()
    }
}

export async function preLoadChat(characterIndex:number, chatIndex:number){
    const character = DBState.db?.characters?.[characterIndex]
    const chat = character?.chats?.[chatIndex]

    if(!chat || get(selectedCharID) !== characterIndex || character.chatPage !== chatIndex){
        return
    }

    if(chat.message?.[0]?.data?.startsWith(coldStorageHeader)){
        const navigationGeneration = getPersistentNavigationGeneration()
        const session = getActiveConversationSession()
        if (session && !session.matchesConversation(character.chaId, chat)) return
        const sessionVersion = session?.version
        const sessionToken = session?.positionAt(0).sessionToken
        const expectedMessages = chat.message
        const expectedPlaceholder = chat.message[0]
        const characterId = character.chaId
        const conversationId = chat.id
        //bring back from cold storage
        const coldDataKey = chat.message[0].data.slice(coldStorageHeader.length)
        const coldData = await getColdStorageItem(coldDataKey)
        const replacement = { ...chat }
        if(coldData && Array.isArray(coldData)){
            replacement.message = coldData
            replacement.lastDate = Date.now()
        }
        else if(coldData?.message){
            replacement.message = coldData.message
            if (Object.hasOwn(coldData, 'savedToggleValues'))
                replacement.savedToggleValues = coldData.savedToggleValues
            if (Object.hasOwn(coldData, 'bindedPersona')) replacement.bindedPersona = coldData.bindedPersona
            replacement.hypaV2Data = coldData.hypaV2Data
            replacement.hypaV3Data = coldData.hypaV3Data
            replacement.scriptstate = coldData.scriptstate
            replacement.localLore = coldData.localLore
            replacement.lastDate = Date.now()
        }
        else{
            // Cold storage data is missing or corrupted.
            // Replace with an error message so the user knows what happened
            // instead of silently showing a broken pointer.
            console.error(`Cold storage data not found for key: ${coldDataKey}`)
            replacement.message = [{
                time: Date.now(),
                data: `[Cold storage data could not be loaded. Key: ${coldDataKey}]`,
                role: 'char'
            }]
            replacement.lastDate = Date.now()
        }
        const currentCharacter = DBState.db?.characters?.[characterIndex]
        const currentChat = currentCharacter?.chats?.[chatIndex]
        if (
            getPersistentNavigationGeneration() !== navigationGeneration ||
            get(selectedCharID) !== characterIndex ||
            currentCharacter !== character ||
            currentCharacter.chaId !== characterId ||
            currentCharacter.chatPage !== chatIndex ||
            currentChat !== chat ||
            currentChat.id !== conversationId ||
            currentChat.message !== expectedMessages ||
            currentChat.message[0] !== expectedPlaceholder ||
            currentChat.message[0]?.data !== `${coldStorageHeader}${coldDataKey}`
        ) return
        const currentSession = getActiveConversationSession()
        if (
            session &&
            sessionVersion !== undefined &&
            sessionToken !== undefined &&
            currentSession === session &&
            session.version === sessionVersion &&
            session.ownsSessionToken(sessionToken) &&
            session.matchesConversation(characterId, chat)
        ) {
            session.adoptConversationReplacement(
                sessionVersion,
                expectedMessages,
                replacement,
            )
            return
        }
        if (session || currentSession) return
        Object.assign(chat, replacement)
    }

}
