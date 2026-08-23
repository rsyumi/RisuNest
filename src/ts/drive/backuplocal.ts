import { BaseDirectory, writeFile } from "@tauri-apps/plugin-fs";
import localforage from "localforage";
import { alertError, alertNormal, alertStore, alertWait, alertMd, alertConfirm } from "../alert";
import { getUncleanables, LocalWriter, forageStorage } from "../globalApi.svelte";
import { resolveBlobStore } from "../storage/platformBlobStore";
import type { InlayBlobMetadata } from "../storage/blobStore";
import {
    collectBackupAssetKeys,
    decodeBackupInlayEntry,
    encodeBackupInlayEntry,
    getBackupInlayName,
    isLegacyBackupAssetKey,
    readBackupAsset,
    selectLegacyBackupAssetKeys,
    writeBackupAsset,
} from "./backupAssets";
import { classifyPocketRisuEntry, PocketRisuInlayImporter } from "./pocketRisuBackup";
import { isTauri, isTauriDesktop } from "src/ts/platform"
import { decodeRisuSave, encodeRisuSaveLegacy } from "../storage/risuSave";
import { getDatabase } from "../storage/database.svelte";
import { relaunch } from "@tauri-apps/plugin-process";
import { decryptBuffer, encryptBuffer, sleep } from "../util";
import { hubURL } from "../characterCards";
import { language } from "src/lang";
import { collectColdStorageBackupPayloads, confirmIncompleteColdStorageOperation, getColdStorageBackupKey, getColdStorageItem, isColdStorageBackupData, listColdDataKeys, setLocalColdStorageItem } from "../process/coldstorage.svelte";
import { publishCurrentOfficialRevision, replacePersistentDatabase } from "../storage/persistentDataRuntime.svelte";
import { installLocalBackup } from "../storage/databaseRestore";

function getBasename(data:string){
    const baseNameRegex = /\\/g
    const splited = data.replace(baseNameRegex, '/').split('/')
    const lasts = splited[splited.length-1]
    return lasts
}

export async function SaveLocalBackup(){
    await forageStorage.Init()
    const blobStore = await resolveBlobStore()
    alertWait("Saving local backup...")
    const db = getDatabase()
    const coldStoragePayloads = await collectColdStorageBackupPayloads(db)
    const unavailableColdStorageKeys = [...coldStoragePayloads.missingKeys, ...coldStoragePayloads.invalidKeys]
    if(!await confirmIncompleteColdStorageOperation(db, unavailableColdStorageKeys, 'backup')){
        return
    }

    const writer = new LocalWriter()
    const r = await writer.init('RisuAI Backup', ['bin'], 'risu-backup.bin')
    if(!r){
        alertError('Failed')
        return
    }

    const assetMap = new Map<string, { charName: string, assetName: string }>()
    if (db.characters) {
        for (const char of db.characters) {
            if (!char) continue
            const charName = char.name ?? 'Unknown Character'
            
            if (char.image) assetMap.set(char.image, { charName: charName, assetName: 'Main Image' })
            
            if (char.emotionImages) {
                for (const em of char.emotionImages) {
                    if (em && em[1]) assetMap.set(em[1], { charName: charName, assetName: em[0] })
                }
            }
            if (char.type !== 'group') {
                if (char.additionalAssets) {
                    for (const em of char.additionalAssets) {
                        if (em && em[1]) assetMap.set(em[1], { charName: charName, assetName: em[0] })
                    }
                }
                if (char.vits) {
                    const keys = Object.keys(char.vits.files)
                    for (const key of keys) {
                        const vit = char.vits.files[key]
                        if (vit) assetMap.set(vit, { charName: charName, assetName: key })
                    }
                }
                if (char.ccAssets) {
                    for (const asset of char.ccAssets) {
                        if (asset && asset.uri) assetMap.set(asset.uri, { charName: charName, assetName: asset.name })
                    }
                }
            }
        }
    }
    if (db.userIcon) {
        assetMap.set(db.userIcon, { charName: 'User Settings', assetName: 'User Icon' })
    }
    if (db.customBackground) {
        assetMap.set(db.customBackground, { charName: 'User Settings', assetName: 'Custom Background' })
    }
    const missingAssets: string[] = []

    const backupAssetKeys = await collectBackupAssetKeys(
        blobStore,
        await getUncleanables(db, 'pure'),
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
            if (db.skipSavingAssetsOnWebSync) {
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

    const inlays = (await blobStore.list({ kind: 'inlay' }))
        .filter((metadata): metadata is InlayBlobMetadata =>
            metadata.kind === 'inlay' && !isLegacyBackupAssetKey(metadata.key))
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
        await writer.writeBackup(payload.backupName, payload.encoded)
    }

    const dbWithoutAccount = { ...db, account: undefined }
    let dbData = encodeRisuSaveLegacy(dbWithoutAccount, 'compression')

    if(forageStorage.isAccount && location.origin.endsWith('risuai.xyz')){
        const time = Date.now()
        const key = (await (await fetch(`https://sv.risuai.xyz/cryptokey?key=${time}`)).json()).key
        const encrypted = await encryptBuffer(dbData, key)
        await writer.writeBackup('encryption.risudat', new TextEncoder().encode(JSON.stringify({ time, type: 'account' })))
        dbData = new Uint8Array(encrypted)
    }

    alertWait(`Saving local Backup... (Saving database)`) 

    await writer.writeBackup('database.risudat', dbData)
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
    await forageStorage.Init()
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
    const db = getDatabase()
    const coldStoragePayloads = await collectColdStorageBackupPayloads(db)
    const unavailableColdStorageKeys = [...coldStoragePayloads.missingKeys, ...coldStoragePayloads.invalidKeys]
    if(!await confirmIncompleteColdStorageOperation(db, unavailableColdStorageKeys, 'backup')){
        return
    }

    const writer = new LocalWriter()
    const r = await writer.init('RisuAI Backup', ['bin'], 'risu-partial-backup.bin')
    if(!r){
        alertError('Failed')
        return
    }

    const assetMap = new Map<string, { charName: string, assetName: string }>()
    
    // Only collect main profile images for both characters and groups
    if (db.characters) {
        for (const char of db.characters) {
            if (!char) continue
            const charName = char.name ?? 'Unknown Character'
            
            // Save the main profile image (supports both character and group types)
            // Note: emotionImages are intentionally excluded from partial backup
            if (char.image) {
                assetMap.set(char.image, { charName: charName, assetName: 'Profile Image' })
            }
        }
    }
    
    // User icon
    if (db.userIcon) {
        assetMap.set(db.userIcon, { charName: 'User Settings', assetName: 'User Icon' })
    }
    
    // Persona icons
    if (db.personas) {
        for (const persona of db.personas) {
            if (persona && persona.icon) {
                assetMap.set(persona.icon, { charName: 'Persona', assetName: `${persona.name} Icon` })
            }
        }
    }
    
    // Custom background
    if (db.customBackground) {
        assetMap.set(db.customBackground, { charName: 'User Settings', assetName: 'Custom Background' })
    }
    
    // Folder images in characterOrder
    if (db.characterOrder) {
        for (const item of db.characterOrder) {
            if (typeof item !== 'string' && item.img) {
                assetMap.set(item.img, { charName: 'Folder', assetName: `${item.name} Folder Image` })
            }
            if (typeof item !== 'string' && item.imgFile) {
                assetMap.set(item.imgFile, { charName: 'Folder', assetName: `${item.name} Folder Image File` })
            }
        }
    }
    
    // Bot preset images
    if (db.botPresets) {
        for (const preset of db.botPresets) {
            if (preset && preset.image) {
                assetMap.set(preset.image, { charName: 'Preset', assetName: `${preset.name} Preset Image` })
            }
        }
    }
    
    const missingAssets: string[] = []

    const assetKeys = selectLegacyBackupAssetKeys(Array.from(assetMap.keys()))
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
        await writer.writeBackup(payload.backupName, payload.encoded)
    }

    const dbWithoutAccount = { ...db, account: undefined }
    const dbData = encodeRisuSaveLegacy(dbWithoutAccount, 'compression')

    alertWait(`Saving partial local backup... (Saving database)`) 

    await writer.writeBackup('database.risudat', dbData)
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
            await forageStorage.Init()
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
