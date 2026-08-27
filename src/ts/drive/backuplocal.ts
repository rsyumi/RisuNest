import { BaseDirectory, open, writeFile } from "@tauri-apps/plugin-fs";
import localforage from "localforage";
import { alertError, alertNormal, alertStore, alertWait, alertMd, alertConfirm } from "../alert";
import { LocalWriter, forageStorage } from "../globalApi.svelte";
import { resolveBlobStore } from "../storage/platformBlobStore";
import type { BlobStore } from "../storage/blobStore";
import {
    collectBackupAssetKeys,
    collectReferencedBackupInlays,
    createColdStorageReferenceDatabase,
    decodeBackupInlayEntry,
    encodeBackupInlayEntry,
    getBackupInlayName,
    isLegacyBackupAssetKey,
    readBackupAsset,
    scanPinnedBackupRecords,
    writeBackupAsset,
} from "./backupAssets";
import { classifyPocketRisuEntry, PocketRisuInlayImporter } from "./pocketRisuBackup";
import { isTauri, isTauriDesktop } from "src/ts/platform"
import { decodeRisuSave } from "../storage/risuSave";
import { relaunch } from "@tauri-apps/plugin-process";
import { decryptBuffer, encryptBuffer, sleep } from "../util";
import { hubURL } from "../characterCards";
import { language } from "src/lang";
import { collectColdStorageBackupPayloads, confirmIncompleteColdStorageOperation, getColdStorageBackupKey, getColdStorageItem, isColdStorageBackupData, listColdDataKeys, setLocalColdStorageItem } from "../process/coldstorage.svelte";
import { getPersistentDataRuntime, publishCurrentOfficialRevision, replacePersistentDatabase } from "../storage/persistentDataRuntime.svelte";
import { installLocalBackup } from "../storage/databaseRestore";
import { type PinnedRisuSaveExport, withFlushedRisuSaveExport } from "../storage/risuSaveStoreAdapter";

const NATIVE_BACKUP_READ_BYTES = 1024 * 1024

export async function* streamNativeBackupFile(
    path: string,
    byteLength: number,
): AsyncGenerator<Uint8Array> {
    let file: Awaited<ReturnType<typeof open>> | undefined
    let primaryError: unknown
    try {
        file = await open(path, { read: true })
        const buffer = new Uint8Array(NATIVE_BACKUP_READ_BYTES)
        let readBytes = 0
        while (true) {
            const length = await file.read(buffer)
            if (length === null) break
            if (length <= 0 || length > buffer.byteLength) {
                throw new Error('Native backup source returned an invalid read length')
            }
            if (readBytes + length > byteLength) {
                throw new Error('Native backup source exceeded its declared length')
            }
            readBytes += length
            yield buffer.slice(0, length)
        }
        if (readBytes !== byteLength) {
            throw new Error('Native backup source ended before its declared length')
        }
    } catch (error) {
        primaryError = error
        throw error
    } finally {
        if (file) {
            try {
                await file.close()
            } catch (error) {
                if (primaryError === undefined) throw error
            }
        }
    }
}

type LocalBackupDatabaseWriter = Pick<LocalWriter, 'writeBackup' | 'writeBackupStream'>

export function writePinnedLocalBackupDatabase(
    writer: LocalBackupDatabaseWriter,
    pinned: PinnedRisuSaveExport,
): Promise<void> {
    if (pinned.withNativeFile) {
        return pinned.withNativeFile(
            { omitAccount: true },
            (file) => writer.writeBackupStream(
                'database.risudat',
                file.bytes,
                streamNativeBackupFile(file.path, file.bytes),
            ),
        )
    }
    return pinned.collectBytes({ omitAccount: true }).then((bytes) => (
        writer.writeBackup('database.risudat', bytes)
    ))
}

function getBasename(data:string){
    const baseNameRegex = /\\/g
    const splited = data.replace(baseNameRegex, '/').split('/')
    const lasts = splited[splited.length-1]
    return lasts
}

export async function SaveLocalBackup(){
    if (isTauri) {
        try {
            const { exportLegacyLocalBackupFromSystemPicker } = await import(
                './legacyLocalBackupFileRouteProduction.svelte'
            )
            const result = await exportLegacyLocalBackupFromSystemPicker()
            if (result) alertNormal('Success')
            return result
        } catch (error) {
            if (error instanceof DOMException && error.name === 'AbortError') return
            if (!isNativeLegacyBackupFallback(error)) throw error
        }
    }
    return saveLocalBackupWithWebView()
}

function isNativeLegacyBackupFallback(error: unknown): boolean {
    if (typeof error !== 'object' || error === null || !('code' in error)) return false
    return error.code === 'capability-unavailable' || error.code === 'unsupported-format'
}

async function saveLocalBackupWithWebView(){
    if (!isTauri) await forageStorage.Init()
    const blobStore = await resolveBlobStore()
    alertWait("Saving local backup...")
    return withFlushedRisuSaveExport(
        getPersistentDataRuntime(),
        'local-backup',
        (pinned) => saveLocalBackupSnapshot(blobStore, pinned),
    )
}

async function saveLocalBackupSnapshot(blobStore: BlobStore, pinned: PinnedRisuSaveExport) {
    const { root, accumulator } = await scanPinnedBackupRecords(pinned.reader, 'full')
    const coldReferenceDatabase = createColdStorageReferenceDatabase(
        accumulator.finish().coldCharacterReferences,
    )
    const coldStoragePayloads = await collectColdStorageBackupPayloads(coldReferenceDatabase)
    const unavailableColdStorageKeys = [...coldStoragePayloads.missingKeys, ...coldStoragePayloads.invalidKeys]
    if(!await confirmIncompleteColdStorageOperation(
        coldReferenceDatabase,
        unavailableColdStorageKeys,
        'backup',
    )){
        return
    }
    for (const payload of coldStoragePayloads.payloads) accumulator.visitColdPayload(payload.value)
    const references = accumulator.finish()

    const writer = new LocalWriter()
    const r = await writer.init('RisuAI Backup', ['bin'], 'risu-backup.bin')
    if(!r){
        alertError('Failed')
        return
    }

    const assetMap = references.assetLabels
    const missingAssets: string[] = []

    const backupAssetKeys = await collectBackupAssetKeys(
        blobStore,
        references.assetKeys,
    )
    for(let i=0;i<backupAssetKeys.length;i++){
        const key = backupAssetKeys[i]
        let message = `Saving local Backup... (${i + 1} / ${backupAssetKeys.length})`
        if (missingAssets.length > 0) {
            const skippedItems = missingAssets.map(key => {
                const assetInfo = assetMap.get(key);
                return assetInfo ? `'${assetInfo.assetName}' from ${assetInfo.charName}` : `'${key}'`;
            }).join(', ');
            message += `\n(Skipping... ${skippedItems})`;
        }
        alertWait(message)

        let data = await blobStore.read(key)
        let readRemotely = false
        if (data === null && forageStorage.isAccount) {
            if (root.skipSavingAssetsOnWebSync) {
                continue
            }
            data = await readBackupAsset(blobStore, key, true)
            readRemotely = true
        }
        if (data) {
            await writer.writeBackup(isTauri ? key.slice('assets/'.length) : key, data)
        } else {
            missingAssets.push(key)
        }
        if (readRemotely) {
            await sleep(1000)
        }
    }

    const inlays = await collectReferencedBackupInlays(blobStore, references.inlayKeys)
    for(let i=0;i<inlays.length;i++){
        const metadata = inlays[i]
        alertWait(`Saving local Backup inlays... (${i + 1} / ${inlays.length})`)
        const data = await blobStore.read(metadata.key)
        if (data === null) {
            missingAssets.push(metadata.key)
            continue
        }
        await writer.writeBackup(
            getBackupInlayName(metadata.key),
            encodeBackupInlayEntry(metadata, data),
        )
    }

    for(let i=0;i<coldStoragePayloads.payloads.length;i++){
        const payload = coldStoragePayloads.payloads[i]
        let message = `Saving local Backup Cold data... (${i + 1} / ${coldStoragePayloads.payloads.length})`
        alertWait(message)
        const encoded = new TextEncoder().encode(JSON.stringify(payload.value))
        await writer.writeBackup(payload.backupName, encoded)
    }

    alertWait(`Saving local Backup... (Saving database)`)

    if(forageStorage.isAccount && location.origin.endsWith('risuai.xyz')){
        const dbData = await pinned.collectBytes({ omitAccount: true })
        const time = Date.now()
        const key = (await (await fetch(`https://sv.risuai.xyz/cryptokey?key=${time}`)).json()).key
        const encrypted = await encryptBuffer(dbData, key)
        await writer.writeBackup('encryption.risudat', new TextEncoder().encode(JSON.stringify({ time, type: 'account' })))
        await writer.writeBackup('database.risudat', new Uint8Array(encrypted))
    } else {
        await writePinnedLocalBackupDatabase(writer, pinned)
    }

    await writer.close()

    if (missingAssets.length > 0) {
        let message = 'Backup Successful, but the following assets were missing and skipped:\n\n'
        for (const key of missingAssets) {
            const assetInfo = assetMap.get(key)
            if (assetInfo) {
                message += `* **${assetInfo.assetName}** (from *${assetInfo.charName}*)  \n  *File: ${key}*\n`
            } else {
                message += `* **Unknown Asset**  \n  *File: ${key}*\n`
            }
        }
        alertMd(message)
    } else {
        alertNormal('Success')
    }
}

/**
 * Saves a partial local backup with only critical assets.
 * 
 * Differences from SaveLocalBackup:
 * - Only includes profile images for characters/groups (excludes emotion images, additional assets, VITS files, CC assets)
 * - Additionally includes: persona icons, folder images, bot preset images
 * - Processes only assets in assetMap (selective) instead of all .png files in assets folder
 * - Faster and more efficient for quick backups
 * - Ideal for backing up core visual identity without bulk data
 */
export async function SavePartialLocalBackup(){
    if (!isTauri) await forageStorage.Init()
    const blobStore = await resolveBlobStore()
    // First confirmation: Explain the difference from regular backup
    const firstConfirm = await alertConfirm(language.partialBackupFirstConfirm)
    
    if (!firstConfirm) {
        return
    }
    
    // Second confirmation: Final warning about not saving assets
    const secondConfirm = await alertConfirm(language.partialBackupSecondConfirm)
    
    if (!secondConfirm) {
        return
    }
    
    alertWait("Saving partial local backup...")
    return withFlushedRisuSaveExport(
        getPersistentDataRuntime(),
        'partial-local-backup',
        (pinned) => savePartialLocalBackupSnapshot(blobStore, pinned),
    )
}

async function savePartialLocalBackupSnapshot(blobStore: BlobStore, pinned: PinnedRisuSaveExport) {
    const { accumulator } = await scanPinnedBackupRecords(pinned.reader, 'partial')
    const references = accumulator.finish()
    const coldReferenceDatabase = createColdStorageReferenceDatabase(
        references.coldCharacterReferences,
    )
    const coldStoragePayloads = await collectColdStorageBackupPayloads(coldReferenceDatabase)
    const unavailableColdStorageKeys = [...coldStoragePayloads.missingKeys, ...coldStoragePayloads.invalidKeys]
    if(!await confirmIncompleteColdStorageOperation(
        coldReferenceDatabase,
        unavailableColdStorageKeys,
        'backup',
    )){
        return
    }

    const writer = new LocalWriter()
    const r = await writer.init('RisuAI Backup', ['bin'], 'risu-partial-backup.bin')
    if(!r){
        alertError('Failed')
        return
    }

    const assetMap = references.assetLabels
    const missingAssets: string[] = []

    const assetKeys = references.assetKeys
    for(let i=0;i<assetKeys.length;i++){
        const key = assetKeys[i]
        let message = `Saving partial local backup... (${i + 1} / ${assetKeys.length})`
        if (missingAssets.length > 0) {
            const skippedItems = missingAssets.map(key => {
                const assetInfo = assetMap.get(key);
                return assetInfo ? `'${assetInfo.assetName}' from ${assetInfo.charName}` : `'${key}'`;
            }).join(', ');
            message += `\n(Skipping... ${skippedItems})`;
        }
        alertWait(message)

        let data = await blobStore.read(key)
        let readRemotely = false
        if (data === null && forageStorage.isAccount) {
            data = await readBackupAsset(blobStore, key, true)
            readRemotely = true
        }
        if (data) {
            await writer.writeBackup(key, data)
        } else {
            missingAssets.push(key)
        }
        if (readRemotely) {
            await sleep(100)
        }
    }

    for(let i=0;i<coldStoragePayloads.payloads.length;i++){
        const payload = coldStoragePayloads.payloads[i]
        let message = `Saving partial local Backup Cold data... (${i + 1} / ${coldStoragePayloads.payloads.length})`
        alertWait(message)
        const encoded = new TextEncoder().encode(JSON.stringify(payload.value))
        await writer.writeBackup(payload.backupName, encoded)
    }

    alertWait(`Saving partial local backup... (Saving database)`) 
    await writePinnedLocalBackupDatabase(writer, pinned)
    await writer.close()

    if (missingAssets.length > 0) {
        let message = 'Partial backup successful, but the following profile images were missing and skipped:\n\n'
        for (const key of missingAssets) {
            const assetInfo = assetMap.get(key)
            if (assetInfo) {
                message += `* **${assetInfo.assetName}** (from *${assetInfo.charName}*)  \n  *File: ${key}*\n`
            } else {
                message += `* **Unknown Asset**  \n  *File: ${key}*\n`
            }
        }
        alertMd(message)
    } else {
        alertNormal('Success')
    }
}

export function LoadLocalBackup(){
    if (isTauri) {
        void loadLocalBackupNativeFirst()
        return
    }
    loadLocalBackupWithWebView()
}

async function loadLocalBackupNativeFirst(): Promise<void> {
    try {
        const { importLegacyLocalBackupFromSystemPicker } = await import(
            './legacyLocalBackupFileRouteProduction.svelte'
        )
        const result = await importLegacyLocalBackupFromSystemPicker()
        if (result) alertNormal('Success')
    } catch (error) {
        if (error instanceof DOMException && error.name === 'AbortError') return
        if (isNativeLegacyBackupFallback(error)) {
            loadLocalBackupWithWebView()
            return
        }
        console.error(error)
        alertError('Failed, Is file corrupted?')
    }
}

function loadLocalBackupWithWebView(){
    try {
        const input = document.createElement('input');
        const encryptionMeta:{
            type: 'none' | 'account';
            time?: number;
        } = {
            type: 'none'
        }
        input.type = 'file';
        input.accept = '.bin';
        input.onchange = async () => {
            if (!input.files || input.files.length === 0) {
                input.remove();
                return;
            }
            const file = input.files[0];
            input.remove();
            if (!isTauri) await forageStorage.Init()
            const blobStore = await resolveBlobStore()
            const pocketRisuInlays = new PocketRisuInlayImporter(async (id, bytes, metadata) => {
                await blobStore.put(id, bytes, metadata)
            })

            const reader = file.stream().getReader();
            const CHUNK_SIZE = 1024 * 1024; // 1MB chunk size
            let bytesRead = 0;
            let remainingBuffer = new Uint8Array();
            let pendingDatabase: Uint8Array | null = null;
            const restoredColdStorageKeys = new Set<string>();

            while (true) {
                const { done, value } = await reader.read();
                if (done) {
                    break;
                }

                bytesRead += value.length;
                const progress = ((bytesRead / file.size) * 100).toFixed(2);
                alertWait(`Loading local Backup... (${progress}%)`);

                const newBuffer = new Uint8Array(remainingBuffer.length + value.length);
                newBuffer.set(remainingBuffer);
                newBuffer.set(value, remainingBuffer.length);
                remainingBuffer = newBuffer;

                let offset = 0;
                while (offset + 4 <= remainingBuffer.length) {
                    const nameLength = new Uint32Array(remainingBuffer.slice(offset, offset + 4).buffer)[0];

                    if (offset + 4 + nameLength > remainingBuffer.length) {
                        break;
                    }
                    const nameBuffer = remainingBuffer.slice(offset + 4, offset + 4 + nameLength);
                    const name = new TextDecoder().decode(nameBuffer);

                    if (offset + 4 + nameLength + 4 > remainingBuffer.length) {
                        break;
                    }
                    const dataLength = new Uint32Array(remainingBuffer.slice(offset + 4 + nameLength, offset + 4 + nameLength + 4).buffer)[0];

                    if (offset + 4 + nameLength + 4 + dataLength > remainingBuffer.length) {
                        break;
                    }
                    const data = remainingBuffer.slice(offset + 4 + nameLength + 4, offset + 4 + nameLength + 4 + dataLength);

                    if( name === 'encryption.risudat') {
                        try {
                            const meta = JSON.parse(new TextDecoder().decode(data)) as typeof encryptionMeta
                            if (meta.type === 'account' && meta.time) {
                                encryptionMeta.type = 'account'
                                encryptionMeta.time = meta.time
                            } else {
                                alertError('Invalid encryption metadata, will attempt to load database backup without decryption.')
                            }
                        } catch (e) {
                            console.error('Failed to parse encryption metadata:', e)
                            alertError('Failed to parse encryption metadata, will attempt to load database backup without decryption.')
                        }
                    }

                    else if (name === 'database.risudat') {
                        pendingDatabase = new Uint8Array(data);
                    }
                    
                    else {
                        const inlayEntry = decodeBackupInlayEntry(name, data)
                        if (inlayEntry) {
                            try {
                                await blobStore.put(inlayEntry.key, inlayEntry.data, inlayEntry.metadata)
                            } catch (e) {
                                console.error(`Failed to restore inlay ${inlayEntry.key}:`, e)
                            }
                            offset += 4 + nameLength + 4 + dataLength;
                            await sleep(10);
                            continue;
                        }
                        const pocketRisuEntry = classifyPocketRisuEntry(name)
                        if (pocketRisuEntry) {
                            if (pocketRisuEntry.kind !== 'skip') {
                                await pocketRisuInlays.add(pocketRisuEntry, new Uint8Array(data))
                            }
                            offset += 4 + nameLength + 4 + dataLength;
                            await sleep(10);
                            continue;
                        }
                        const coldStorageKey = getColdStorageBackupKey(name)
                        let handledAsColdStorage = false

                        if (coldStorageKey) {
                            handledAsColdStorage = true
                            try {
                                const text = new TextDecoder().decode(data)
                                const jsonData = JSON.parse(text)

                                if (isColdStorageBackupData(jsonData)) {
                                    if(await setLocalColdStorageItem(coldStorageKey, jsonData)){
                                        restoredColdStorageKeys.add(coldStorageKey)
                                    } else {
                                        console.error(`Failed to restore cold storage item ${coldStorageKey}`)
                                    }
                                } else {
                                    console.warn(`Skipping invalid cold storage backup item ${name}`)
                                }
                            } catch (e) {
                                console.error(`Failed to parse cold storage item ${coldStorageKey}:`, e)
                            }
                        }

                        if (!handledAsColdStorage) {
                            const key = `assets/${name}`
                            if (isLegacyBackupAssetKey(key)) await writeBackupAsset(blobStore, key, data)
                        }
                    }
                    await sleep(10);

                    offset += 4 + nameLength + 4 + dataLength;
                }
                remainingBuffer = remainingBuffer.slice(offset);
            }

            await pocketRisuInlays.finish()
            if (pocketRisuInlays.failedIds.length > 0) {
                console.error('Failed to import PocketRisu inlays:', pocketRisuInlays.failedIds)
            }

            if(!pendingDatabase){
                alertError('Failed, Is file corrupted?')
                return
            }

            let db = pendingDatabase;
            if(encryptionMeta.type === 'account' && encryptionMeta.time){
                try {
                    const key = (await (await fetch(`https://sv.risuai.xyz/cryptokey?key=${encryptionMeta.time}`)).json()).key
                    const decrypted = await decryptBuffer(db, key)
                    db = new Uint8Array(decrypted)
                }
                catch (e) {
                    console.error('Failed to decrypt database backup:', e)
                    alertError('Failed to decrypt database backup, will attempt to load it without decryption.')
                }
            }
            const dbData = await decodeRisuSave(db);
            const missingColdStorageKeys:string[] = []
            for(const key of await listColdDataKeys(dbData)){
                if(restoredColdStorageKeys.has(key)){
                    continue
                }
                const existingColdStorage = await getColdStorageItem(key, { accountFallback: true })
                if(!isColdStorageBackupData(existingColdStorage)){
                    missingColdStorageKeys.push(key)
                }
            }
            if(!await confirmIncompleteColdStorageOperation(dbData, missingColdStorageKeys, 'restore')){
                return
            }

            await installLocalBackup(dbData, {
                replaceDatabase: replacePersistentDatabase,
                publishAcceptedRevision: publishCurrentOfficialRevision,
                relaunch: async () => {
                    alertStore.set({
                        type: "wait",
                        msg: "Success, Refreshing your app."
                    });
                    // Android has no process relauncher, so the WebView reloads instead.
                    if (isTauriDesktop) {
                        await relaunch();
                    } else {
                        location.search = '';
                    }
                },
            });

            alertNormal('Success');
        };

        input.click();
    } catch (error) {
        console.error(error);
        alertError('Failed, Is file corrupted?')
    }
}
