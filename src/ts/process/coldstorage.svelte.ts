import { DBState } from "../stores.svelte"
import { decompress as fflateDecompress } from "fflate"
import { fetchProtectedResource } from "../sionyw"
import { alertConfirm } from "../alert"
import { language } from "src/lang"
import type { Database } from "../storage/database.svelte"
import { getColdStorageAffectedCharacters, listColdDataKeysFromDb } from "./coldstorageData"

export {
    coldStorageHeader,
    getColdStorageBackupKey,
    getColdStorageBackupName,
    isColdStorageBackupData,
    replaceColdStoragePayloadResources,
    listColdDataKeysFromDb
} from "./coldstorageData"

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

async function discardResponseBody(response:Response):Promise<void> {
    try {
        await response.body?.cancel()
    } catch (error) {}
}

/**
 * Reads an upstream account cold payload. Import paths expand it into a record
 * body; nothing publishes a cold payload back to the account.
 */
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

export async function listColdDataKeys(db: Pick<Database, 'characters'> = DBState.db): Promise<string[]> {
    return listColdDataKeysFromDb(db)
}

export async function confirmIncompleteColdStorageRestore(
    db: Pick<Database, 'characters'>,
    unavailableKeys: Iterable<string>,
): Promise<boolean> {
    const uniqueUnavailableKeys = Array.from(new Set(unavailableKeys))
    if (uniqueUnavailableKeys.length === 0) {
        return true
    }

    const affected = getColdStorageAffectedCharacters(db, uniqueUnavailableKeys)
    return await alertConfirm(language.errors.coldStorageIncompleteRestoreConfirm(
        affected.characterNames.join(', '),
        uniqueUnavailableKeys.length,
        affected.unresolvedKeys.length,
    ))
}
