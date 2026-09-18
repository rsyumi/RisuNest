import { alertError, alertInput, alertNormal, alertSelect, alertStore } from "../alert";
import { getDatabase, type Database } from "../storage/database.svelte";
import { forageStorage, getUncleanablesSync, openURL } from "../globalApi.svelte";
import { resolveBlobStore } from "../storage/platformBlobStore";
import type { BlobStore } from "../storage/blobStore";
import {
    collectBackupAssetKeys,
    collectExactPluginStorageAssetReferences,
    readBackupAsset,
    scanPinnedBackupRecords,
    writeBackupAsset,
} from "./backupAssets";
import { isTauri, isTauriIOS } from "src/ts/platform"
import { restartNativeApp } from "../storage/nativePersistentMaintenance";
import { language } from "../../lang";
import { relaunch } from '@tauri-apps/plugin-process';
import { sleep } from "../util";
import { hubURL } from "../characterCards";
import { getDeviceMarkers } from "../storage/deviceMarkers";
import { decodeRisuSave } from "../storage/risuSave";
import { confirmIncompleteColdStorageRestore, getColdStorageBackupName, isColdStorageBackupData, listColdDataKeys } from "../process/coldstorage.svelte";
import { expandColdPayloads } from "../process/coldPayloadExpansion";
import { getPersistentDataRuntime, publishCurrentOfficialRevision, replacePersistentDatabase } from "../storage/persistentDataRuntime.svelte";
import { installDriveRestore } from "../storage/databaseRestore";
import { externalRestorableSections, externalRestoreAreas } from "../storage/sync/external/restoreScope";
import { type PinnedRisuSaveExport, withFlushedRisuSaveExport } from "../storage/risuSaveStoreAdapter";

async function openExternalGoogleStorageSetup(): Promise<void> {
    const { SettingsMenuIndex, settingsOpen } = await import('../stores.svelte')
    settingsOpen.set(true)
    SettingsMenuIndex.set(17)
    setTimeout(() => {
        window.dispatchEvent(new CustomEvent('risunest:open-external-storage', {
            detail: { providerId: 'google_drive' },
        }))
    })
}

async function chooseExternalGoogleConnection() {
    const { getExternalStorageBridge } = await import('../storage/sync/external/bridge')
    const bridge = getExternalStorageBridge()
    const storage = await bridge.getState()
    const connections = storage.connections.filter(connection => (
        connection.providerId === 'google_drive'
        && connection.status !== 'error'
    ))
    if (connections.length === 0) {
        await openExternalGoogleStorageSetup()
        return null
    }
    if (connections.length === 1) return { bridge, connection: connections[0] }
    const selected = await alertSelect([
        ...connections.map(connection => connection.displayName),
        language.cancel,
    ])
    const selectedIndex = Number(selected)
    if (!Number.isInteger(selectedIndex) || selectedIndex < 0 || selectedIndex >= connections.length) {
        return null
    }
    return { bridge, connection: connections[selectedIndex] }
}

export async function runNativeExternalDriveAction(
    type: 'savetauri' | 'loadtauri',
): Promise<void> {
    const selected = await chooseExternalGoogleConnection()
    if (!selected) return
    const { connection, bridge } = selected
    const { requestExternalStorageNow, requestExternalStorageRestore } = await import(
        '../storage/sync/external/production'
    )
    if (type === 'savetauri') {
        await requestExternalStorageNow(connection.id, 'backup')
        return
    }

    const history = (await bridge.listHistory(connection.id)).items.filter(item => (
        item.complete && item.verified
    ))
    if (history.length === 0) {
        alertNormal(language.risuNest.storage.emptyList)
        return
    }
    const selectedSnapshot = history.length === 1
        ? history[0]
        : history[Number(await alertSelect([
            ...history.map(item => new Date(Number(item.createdAtMs)).toLocaleString()),
            language.cancel,
        ]))]
    if (!selectedSnapshot) return
    await requestExternalStorageRestore(
        connection.id,
        selectedSnapshot.id,
        externalRestoreAreas(selectedSnapshot, externalRestorableSections(selectedSnapshot)),
    )
}

export async function checkDriver(type:'save'|'load'|'loadtauri'|'savetauri'|'reftoken'){
    if (isTauri && (type === 'savetauri' || type === 'loadtauri')) {
        try {
            await runNativeExternalDriveAction(type)
        } catch (error) {
            console.error(error)
            alertError(language.risuNest.backup.actionFailed)
        }
        return
    }
    const CLIENT_ID = '580075990041-l26k2d3c0nemmqiu3d3aag01npfrkn76.apps.googleusercontent.com';
    const REDIRECT_URI = type === 'reftoken' ? 'https://sv.risuai.xyz/drive' : "https://risuai.xyz/"
    const SCOPE = 'https://www.googleapis.com/auth/drive.file https://www.googleapis.com/auth/drive.appdata';
    const encodedRedirectUri = encodeURIComponent(REDIRECT_URI);
    const authorizationUrl = `https://accounts.google.com/o/oauth2/auth?client_id=${CLIENT_ID}&redirect_uri=${encodedRedirectUri}&scope=${SCOPE}&response_type=code&state=${type}`;
    

    if(type === 'reftoken'){
        const authorizationUrl = `https://accounts.google.com/o/oauth2/auth?client_id=${CLIENT_ID}&redirect_uri=${encodedRedirectUri}&scope=${SCOPE}&response_type=code&state=${"accesstauri"}&access_type=offline&prompt=consent`;
        return authorizationUrl
    }

    if(type === 'save' || type === 'load'){
        location.href = (authorizationUrl);
    }
    else{
        
        try {
            if(isTauri){
                openURL(authorizationUrl)
            }
            else{
                window.open(authorizationUrl)
            }
            let code = await alertInput(language.pasteAuthCode)
            if(code.includes(' ')){
                code = code.substring(code.lastIndexOf(' ')).trim()
            }
            if(type === 'loadtauri'){
                await loadDrive(code, 'backup')
            }
            else{
                await backupDrive(code)
            }
        } catch (error) {
            console.error(error)
            alertError(`Backup Error: ${error}`)
        }
    }
}


export async function checkDriverInit() {
    try {
        const loc = new URLSearchParams(location.search)
        const code = loc.get('code')
    
        if(code){
            const res = await fetch(hubURL + `/drive/token?code=${encodeURIComponent(code)}`)
            if(res.status >= 200 && res.status < 300){
                const json:{
                    access_token:string,
                    expires_in:number
                } = await res.json()
                const da = loc.get('state')
                if(da === 'save'){
                    await backupDrive(json.access_token)
                }
                else if(da === 'load'){
                    await loadDrive(json.access_token, 'backup')
                }
                else if(da === 'savetauri' || da === 'loadtauri'){
                    alertStore.set({
                        type: 'wait2',
                        msg: `Copy and paste this Auth Code: ${json.access_token}`
                    })
                }
                else if(da === 'accesstauri'){
                    alertStore.set({
                        type: 'wait2',
                        msg: JSON.stringify(json)
                    })
                }
            }
            else{
                alertError(await res.text())
                // location.search = ''
            }
            return true
        }
        else{
            return false
        }   
    } catch (error) {
        console.error(error)
        alertError(`Backup Error: ${error}`)
        const currentURL = new URL(location.href)
        currentURL.search = ''
        window.history.replaceState( {} , "", currentURL.href );
        await sleep(100000)
        return false
    }
}

let lastSavedCache:number|undefined
const lastSaved = () => lastSavedCache
    ?? (lastSavedCache = parseInt(getDeviceMarkers().getItem('risu_lastsaved') ?? '-1'))

export async function backupDrive(ACCESS_TOKEN:string) {
    if (!isTauri) await forageStorage.Init()
    const blobStore = await resolveBlobStore()
    alertStore.set({
        type: "wait",
        msg: "Uploading Backup..."
    })

    return withFlushedRisuSaveExport(
        getPersistentDataRuntime(),
        'drive-backup',
        (pinned) => backupDriveSnapshot(ACCESS_TOKEN, blobStore, pinned),
    )
}

async function backupDriveSnapshot(
    ACCESS_TOKEN: string,
    blobStore: BlobStore,
    pinned: PinnedRisuSaveExport,
) {
    const { accumulator } = await scanPinnedBackupRecords(pinned.reader, 'full')
    const files:DriveFile[] = await getFilesInFolder(ACCESS_TOKEN)

    const fileNames = files.map((d) => {
        return d.name
    })

    const references = accumulator.finish()

    const assetKeys = await collectBackupAssetKeys(
        blobStore,
        references.assetKeys,
    )
    // Drive names are basename-flattened, so two distinct keys (e.g. a flat
    // plugin asset and a nested legacy asset) can map to the same name. Track
    // names written this run so a collision cannot create duplicate Drive
    // files that would silently overwrite each other on restore.
    const uploadedNames = new Set(fileNames)
    for(let i=0;i<assetKeys.length;i++){
        alertStore.set({
            type: "wait",
            msg: `Uploading Backup... (${i + 1} / ${assetKeys.length})`
        })
        const key = assetKeys[i]
        const formatedKey = newFormatKeys(key)
        if(!uploadedNames.has(formatedKey)){
            const data = await readBackupAsset(blobStore, key, forageStorage.isAccount)
            if (data) {
                await createFileInFolder(ACCESS_TOKEN, formatedKey, data)
                uploadedNames.add(formatedKey)
            }
        }
    }

    const dbData = await pinned.collectBytes()

    alertStore.set({
        type: "wait",
        msg: `Uploading Backup... (Saving database)`
    })

    await createFileInFolder(ACCESS_TOKEN, `${(Date.now() / 1000).toFixed(0)}-database.risudat`, dbData)


    alertNormal('Success')
}

type DriveFile = {
    mimeType:string
    name:string
    id: string
}

export async function loadDrive(ACCESS_TOKEN:string, mode: 'backup'|'sync'):Promise<void|"noSync"> {
    if (!isTauri) await forageStorage.Init()
    const blobStore = await resolveBlobStore()
    if(mode === 'backup'){
        alertStore.set({
            type: "wait",
            msg: "Loading Backup..."
        })
    }
    const files:DriveFile[] = await getFilesInFolder(ACCESS_TOKEN)
    let db = getDatabase()

    async function checkImageExists(images:string) {
        if(db?.account?.useSync){
            return false
        }
        const key = `assets/${images}`
        if (await blobStore.stat(key)) return true
        return forageStorage.isAccount
            && await readBackupAsset(blobStore, key, true) !== null
    }
    const fileNames = files.map((d) => {
        return d.name
    })


    let dbs:[DriveFile,number][] = []
    let noSyncData = true

    if(mode === 'backup'){
        for(const f of files){
            if(f.name.endsWith("-database.risudat")){
                const tm = parseInt(f.name.split('-')[0])
                if(isNaN(tm)){
                    continue
                }
                else{
                    dbs.push([f,tm])
                }
            }
        }
        dbs.sort((a,b) => {
            return b[1] - a[1]
        })
    }
    else if(mode === 'sync'){
        for(const f of files){
            if(f.name.endsWith("-database.risudat2")){
                const tm = parseInt(f.name.split('-')[0])
                if(isNaN(tm)){
                    continue
                }
                else{
                    if(tm > lastSaved()){
                        dbs.push([f,tm])
                    }
                    noSyncData = false
                }
            }
        }
        dbs.sort((a,b) => {
            return b[1] - a[1]
        })
    }

    if(noSyncData && mode === 'sync'){
        return 'noSync'
    }

    if(dbs.length !== 0){
        if(mode === 'sync'){
            alertStore.set({
                type: "wait",
                msg: "Sync Data..."
            })
        }
        async function getDbFromList(){
            let selectables:string[] = []
            for(let i=0;i<dbs.length;i++){
                selectables.push(`Backup saved in ${(new Date(dbs[i][1] * 1000)).toLocaleString()}`)
                if(selectables.length > 7){
                    break
                }
            }
            const selectedIndex = (await alertSelect([language.loadLatest, language.loadOthers]) === '0') ? 0 : parseInt(await alertSelect(selectables))
            const selectedDb = dbs[selectedIndex][0]
            const decompressedDb:Database = await decodeRisuSave(await getFileData(ACCESS_TOKEN, selectedDb.id))
            return decompressedDb
        }
    
        const db:Database = mode === 'backup' ? await getDbFromList() : JSON.parse(Buffer.from(await getFileData(ACCESS_TOKEN, dbs[0][0].id)).toString('utf-8'))
        const coldStorage = await readColdStorageFromDrive(ACCESS_TOKEN, files, db, mode)
        if(coldStorage.failures.length > 0){
            if(mode === 'sync'){
                alertError(`Sync failed. ${coldStorage.failures.length} cold storage item(s) could not be restored.`)
                return
            }
            if(!await confirmIncompleteColdStorageRestore(db, coldStorage.failures)){
                return
            }
        }
        await expandColdPayloads(db, async (key) => coldStorage.payloads.get(key) ?? null)
        const requiredImages = await getDriveRestoreRequiredImages(db)
        let ind = 0;
        let errorLogs:string[] = []
        for(const images of requiredImages){
            ind += 1
            for(let tries=0;tries<3;tries++){
                const formatedImage = tries === 0 ? newFormatKeys(images) : formatKeys(images)
                if(mode === 'sync'){
                    alertStore.set({
                        type: "wait",
                        msg: `Sync Files... (${ind} / ${requiredImages.length})`
                    })
                }
                else{
                    alertStore.set({
                        type: "wait",
                        msg: `Loading Backup... (${ind} / ${requiredImages.length})`
                    })
                }
                if(await checkImageExists(images)){
                    //skip process
                }
                else{
                    if(formatedImage.length >= 7){
                        if(fileNames.includes(formatedImage)){
                            for(const file of files){
                                if(file.name === formatedImage){
                                    const fData = await getFileData(ACCESS_TOKEN, file.id)
                                    await writeBackupAsset(blobStore, `assets/${images}`, fData)
                                    tries = 3
                                    // Older backups could hold duplicate names;
                                    // take the first match deterministically.
                                    break
                                }
                            }
                        }
                        else{
                            alertStore.set({
                                type: "wait",
                                msg: `Loading Backup... (${ind} / ${requiredImages.length}) (Error in ${formatedImage})`
                            })
                            await sleep(1000)
                        }
                    }
                }
            }
        }
        db.didFirstSetup = true
        await installDriveRestore(db, {
            replaceDatabase: replacePersistentDatabase,
            onPostCommitError: (error) => {
                console.error('Committed Drive restore follow-up failed', error)
                alertError(language.risuNest.persistentData.followupFailed)
            },
            publishAcceptedRevision: publishCurrentOfficialRevision,
            relaunch: async () => {
                lastSavedCache = Date.now()
                const markers = getDeviceMarkers()
                markers.setItem('risu_lastsaved', `${lastSavedCache}`)
                await markers.flush()
                alertStore.set({
                    type: "wait",
                    msg: "Success, Refreshing your app."
                })
                if (isTauriIOS) {
                    await restartNativeApp()
                } else if (isTauri) {
                    await relaunch()
                } else {
                    location.search = ''
                }
            },
        })
    }
    else if(mode === 'backup'){
        location.search = ''
    }
}

async function getDriveRestoreRequiredImages(db:Database):Promise<string[]> {
    const required = new Set(getUncleanablesSync(db, 'basename'))
    for (const key of collectExactPluginStorageAssetReferences(db.pluginCustomStorage ?? {})) {
        required.add(getBasename(key))
    }
    return [...required]
}

async function readColdStorageFromDrive(
    ACCESS_TOKEN:string,
    files:DriveFile[],
    db:Database,
    mode:'backup'|'sync'
) {
    const coldKeys = await listColdDataKeys(db)
    const failures:string[] = []
    const payloads = new Map<string, unknown>()
    for(let i=0;i<coldKeys.length;i++){
        const key = coldKeys[i]
        const names = new Set([
            getColdStorageBackupName(key),
            `coldstorage/${key}.json`,
            `${key}.json`
        ])
        const file = files.find((driveFile) => names.has(driveFile.name))
        if(!file){
            console.warn(`Cold storage data not found in Drive backup: ${key}`)
            failures.push(key)
            continue
        }
        alertStore.set({
            type: "wait",
            msg: `${mode === 'sync' ? 'Sync' : 'Loading Backup'} Cold Storage... (${i + 1} / ${coldKeys.length})`
        })
        try {
            const jsonData = JSON.parse(new TextDecoder().decode(await getFileData(ACCESS_TOKEN, file.id)))
            if(isColdStorageBackupData(jsonData)){
                payloads.set(key, jsonData)
            }
            else{
                console.warn(`Skipping invalid cold storage Drive item ${file.name}`)
                failures.push(key)
            }
        } catch (error) {
            console.error(`Failed to restore cold storage item ${key}:`, error)
            failures.push(key)
        }
    }
    return { payloads, failures }
}

function checkImageExist(image:string){

}


function formatKeys(name:string) {
    return getBasename(name).replace(/\_/g, '__').replace(/\./g,'_d').replace(/\//,'_s') + '.png'
}

function newFormatKeys(name:string) {
    let n = getBasename(name)
    const bf = Buffer.from(n).toString('hex')
    return n + '.bin'
}

async function getFilesInFolder(ACCESS_TOKEN:string, nextPageToken=''): Promise<DriveFile[]> {
    const url = `https://www.googleapis.com/drive/v3/files?spaces=appDataFolder&pageSize=300` + nextPageToken;
    
    const response = await fetch(url, {
        method: 'GET',
        headers: {
            'Authorization': `Bearer ${ACCESS_TOKEN}`,
            'Content-Type': 'application/json',
        },
    });
    
    if (response.ok) {
        const data = await response.json();
        if(data.nextPageToken){
            return (data.files as DriveFile[]).concat(await getFilesInFolder(ACCESS_TOKEN, `&pageToken=${data.nextPageToken}`))
        }
        return data.files as DriveFile[];
    } else {
        throw(`Error: ${response.status}`);
    }
}

async function createFileInFolder(accessToken:string, fileName:string, content:Uint8Array, mimeType = 'application/octet-stream') {
    const metadata = {
      name: fileName,
      mimeType: mimeType,
      parents: ["appDataFolder"],
    };
  
    const body = new FormData();
    body.append(
      "metadata",
      new Blob([JSON.stringify(metadata)], { type: "application/json" })
    );
    body.append("file", new Blob([content as any], { type: mimeType }));
  
    const response = await fetch(
      "https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart",
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${accessToken}`,
        },
        body: body,
      }
    );
  
    const result = await response.json();
  
    if (response.ok) {
      return result;
    } else {
      console.error("Error creating file:", result);
      throw new Error(result.error.message);
    }
}
  
const baseNameRegex = /\\/g
function getBasename(data:string){
    const splited = data.replace(baseNameRegex, '/').split('/')
    const lasts = splited[splited.length-1]
    return lasts
}

async function getFileData(ACCESS_TOKEN:string,fileId:string) {
    const url = `https://www.googleapis.com/drive/v3/files/${fileId}?alt=media`;
  
    const request = {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${ACCESS_TOKEN}`
      }
    };
  
    const response = await fetch(url, request);
  
    if (response.ok) {
      const data = new Uint8Array(await response.arrayBuffer());
      return data;
    } else {
        throw "Error in response when reading files in folder"
    }
  }
