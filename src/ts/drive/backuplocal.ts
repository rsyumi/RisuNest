import { BaseDirectory, open, writeFile } from "@tauri-apps/plugin-fs";
import localforage from "localforage";
import { alertError, alertNormal, alertWait, alertMd, alertConfirm } from "../alert";
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
import { emptyNativeImportCounts, NativeFileJobActivationCommittedError, NativeFileJobError, syntheticNativeFileJobStatus, type NativeFileJobStage } from "../storage/nativeFileJobs";
import { recordNativeLogError } from "../nativeLog";
import { NativeFileOperationBusyError, runSharedNativeFileOperation } from "../storage/nativeFileJobManager";
import { isNativeLegacyBackupFallback, type LegacyLocalBackupFallbackContext } from "./legacyLocalBackupFileRoute";

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
    const r = await writer.init('RisuNest Backup', ['bin'], 'risu-backup.bin')
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
    // Archive entry names are basename-flattened by writeBackup, so distinct
    // keys (e.g. a flat plugin asset and a nested legacy asset) can collide.
    // Skip duplicates so restore cannot silently overwrite one with the other.
    const writtenAssetNames = new Set<string>()
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
            const entryBasename = getBasename(key)
            if (!writtenAssetNames.has(entryBasename)) {
                writtenAssetNames.add(entryBasename)
                await writer.writeBackup(isTauri ? key.slice('assets/'.length) : key, data)
            } else {
                console.warn(`Skipping backup asset ${key}: entry name ${entryBasename} already written`)
            }
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
    const r = await writer.init('RisuNest Backup', ['bin'], 'risu-partial-backup.bin')
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

export function LoadLocalBackup(): Promise<void> {
    if (isTauri) return loadLocalBackupNativeFirst()
    return runSharedNativeFileOperation(
        'import',
        'legacy-local-backup-import',
        (context) => importLegacyBackupWithWebView(context),
        { presentation: 'dialog', format: 'local-backup' },
    ).then(() => undefined, handleLegacyImportFailure)
}

async function loadLocalBackupNativeFirst(): Promise<void> {
    try {
        const { importLegacyLocalBackupFromSystemPicker } =
            await import('./legacyLocalBackupFileRouteProduction.svelte')
        await importLegacyLocalBackupFromSystemPicker({ onNativeFallback: importLegacyBackupWithWebView })
    } catch (error) {
        handleLegacyImportFailure(error)
    }
}

/**
 * The progress dialog already shows the outcome to the user; this keeps the
 * diagnostics record and surfaces only the one failure the dialog never sees.
 */
function handleLegacyImportFailure(error: unknown): void {
    if (error instanceof DOMException && error.name === 'AbortError') return
    if (error instanceof NativeFileOperationBusyError) {
        alertError(language.risuNest.backup.actionFailed)
        return
    }
    console.error(error)
    let message: string
    if (error instanceof NativeFileJobActivationCommittedError) {
        const detail = error.cause instanceof Error ? error.cause.message : String(error.cause)
        message = `Backup data was imported, but the app could not refresh. Restart the app before trying another import.\n[${error.code}] ${detail}`
    } else {
        const code = error instanceof NativeFileJobError ? error.code : 'import-error'
        const detail = error instanceof Error ? error.message : String(error)
        message = `Backup import failed [${code}]: ${detail}`
    }
    void recordNativeLogError(message).catch(() => {})
}

const WEB_LEGACY_IMPORT_JOB = {
    jobId: 'web-legacy-local-backup',
    kind: 'restore-legacy-local-backup' as const,
}

function cancelledError(): DOMException {
    return new DOMException('Backup import was cancelled', 'AbortError')
}

function pickWebBackupFile(signal: AbortSignal): Promise<File | null> {
    if (signal.aborted) return Promise.resolve(null)
    return new Promise((resolve) => {
        const input = document.createElement('input')
        input.type = 'file'
        input.accept = '.bin'
        let settled = false
        const finish = (file: File | null) => {
            if (settled) return
            settled = true
            signal.removeEventListener('abort', onAbort)
            input.remove()
            resolve(file)
        }
        const onAbort = () => finish(null)
        input.onchange = async () => {
            finish(input.files?.[0] ?? null)
        }
        input.oncancel = () => finish(null)
        signal.addEventListener('abort', onAbort, { once: true })
        input.click()
    })
}

/**
 * WebView importer for RisuAI and PocketRisu `.bin` backups. Runs inside a
 * dialog-presented operation: it reports every entry it classifies, marks the
 * operation as partially written once anything reaches the blob store, and
 * ends by restarting the app after the database is installed.
 */
export async function importLegacyBackupWithWebView(
    context: LegacyLocalBackupFallbackContext,
): Promise<{ warningCodes: string[] } | null> {
    const file = await pickWebBackupFile(context.signal)
    if (!file) return null
    context.setSource({ name: file.name, bytes: file.size })

    const encryptionMeta: {
        type: 'none' | 'account'
        time?: number
    } = {
        type: 'none',
    }
    if (!isTauri) await forageStorage.Init()
    const blobStore = await resolveBlobStore()
    const counts = emptyNativeImportCounts()
    const warningCodes: string[] = []
    let bytesRead = 0
    let currentItem: string | undefined
    let wroteAnything = false

    const markPartialWrite = () => {
        if (wroteAnything) return
        wroteAnything = true
        context.setPartialWritesPossible(true)
    }
    const markAttachmentWritten = () => {
        markPartialWrite()
        counts.attachmentsPrepared += 1
    }
    const report = (stage: NativeFileJobStage) => {
        context.onStatus(syntheticNativeFileJobStatus(WEB_LEGACY_IMPORT_JOB, stage, {
            stageUnit: 'bytes',
            stageCompleted: bytesRead,
            stageTotal: file.size,
            progress: {
                completedBytes: bytesRead,
                totalBytes: file.size,
                completedItems: counts.entriesRead,
                ...(counts.entriesTotal === undefined ? {} : { totalItems: counts.entriesTotal }),
            },
            counts: { ...counts },
            ...(currentItem === undefined ? {} : { currentItem }),
        }))
    }
    const checkCancelled = () => {
        if (context.signal.aborted) throw cancelledError()
    }
    const pocketRisuInlays = new PocketRisuInlayImporter(async (id, bytes, metadata) => {
        await blobStore.put(id, bytes, metadata)
        markAttachmentWritten()
    })

    report('reading-archive')
    const reader = file.stream().getReader()
    let remainingBuffer = new Uint8Array()
    let pendingDatabase: Uint8Array | null = null
    const restoredColdStorageKeys = new Set<string>()

    while (true) {
        checkCancelled()
        const { done, value } = await reader.read()
        if (done) break
        checkCancelled()

        bytesRead += value.length
        const newBuffer = new Uint8Array(remainingBuffer.length + value.length)
        newBuffer.set(remainingBuffer)
        newBuffer.set(value, remainingBuffer.length)
        remainingBuffer = newBuffer

        let offset = 0
        while (offset + 4 <= remainingBuffer.length) {
            const nameLength = new Uint32Array(remainingBuffer.slice(offset, offset + 4).buffer)[0]
            if (offset + 4 + nameLength > remainingBuffer.length) break
            const nameBuffer = remainingBuffer.slice(offset + 4, offset + 4 + nameLength)
            const name = new TextDecoder().decode(nameBuffer)
            if (offset + 4 + nameLength + 4 > remainingBuffer.length) break
            const dataLength = new Uint32Array(
                remainingBuffer.slice(offset + 4 + nameLength, offset + 4 + nameLength + 4).buffer,
            )[0]
            if (offset + 4 + nameLength + 4 + dataLength > remainingBuffer.length) break
            const data = remainingBuffer.slice(
                offset + 4 + nameLength + 4,
                offset + 4 + nameLength + 4 + dataLength,
            )
            const entryLength = 4 + nameLength + 4 + dataLength

            checkCancelled()
            counts.entriesRead += 1
            currentItem = name
            report('reading-archive')

            if (name === 'encryption.risudat') {
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
            } else if (name === 'database.risudat') {
                pendingDatabase = new Uint8Array(data)
            } else {
                const inlayEntry = decodeBackupInlayEntry(name, data)
                if (inlayEntry) {
                    counts.inlays += 1
                    try {
                        await blobStore.put(inlayEntry.key, inlayEntry.data, inlayEntry.metadata)
                        markAttachmentWritten()
                    } catch (e) {
                        console.error(`Failed to restore inlay ${inlayEntry.key}:`, e)
                        counts.skipped += 1
                    }
                    offset += entryLength
                    await sleep(10)
                    continue
                }
                const pocketRisuEntry = classifyPocketRisuEntry(name)
                if (pocketRisuEntry) {
                    if (pocketRisuEntry.kind === 'skip') {
                        counts.skipped += 1
                    } else {
                        if (pocketRisuEntry.kind === 'inlay-data' || pocketRisuEntry.kind === 'inlay-legacy') {
                            counts.pocketMedia += 1
                        } else {
                            counts.pocketMetadata += 1
                        }
                        await pocketRisuInlays.add(pocketRisuEntry, new Uint8Array(data))
                    }
                    offset += entryLength
                    await sleep(10)
                    continue
                }
                const coldStorageKey = getColdStorageBackupKey(name)
                let handledAsColdStorage = false

                if (coldStorageKey) {
                    handledAsColdStorage = true
                    counts.coldStorage += 1
                    try {
                        const text = new TextDecoder().decode(data)
                        const jsonData = JSON.parse(text)

                        if (isColdStorageBackupData(jsonData)) {
                            if (await setLocalColdStorageItem(coldStorageKey, jsonData)) {
                                restoredColdStorageKeys.add(coldStorageKey)
                                markPartialWrite()
                            } else {
                                console.error(`Failed to restore cold storage item ${coldStorageKey}`)
                                counts.skipped += 1
                            }
                        } else {
                            console.warn(`Skipping invalid cold storage backup item ${name}`)
                            counts.skipped += 1
                        }
                    } catch (e) {
                        console.error(`Failed to parse cold storage item ${coldStorageKey}:`, e)
                        counts.skipped += 1
                    }
                }

                if (!handledAsColdStorage) {
                    const key = `assets/${name}`
                    if (isLegacyBackupAssetKey(key)) {
                        counts.assets += 1
                        await writeBackupAsset(blobStore, key, data)
                        markAttachmentWritten()
                    } else {
                        counts.skipped += 1
                    }
                }
            }
            await sleep(10)

            offset += entryLength
        }
        remainingBuffer = remainingBuffer.slice(offset)
    }

    await pocketRisuInlays.finish()
    if (pocketRisuInlays.failedIds.length > 0) {
        console.error('Failed to import PocketRisu inlays:', pocketRisuInlays.failedIds)
        counts.skipped += pocketRisuInlays.failedIds.length
        warningCodes.push('pocket-inlay-failed')
    }
    counts.entriesTotal = counts.entriesRead
    currentItem = undefined

    if (!pendingDatabase) {
        throw new NativeFileJobError('invalid-source', 'The backup file has no database entry')
    }
    checkCancelled()
    report('decoding-database')

    let db = pendingDatabase
    if (encryptionMeta.type === 'account' && encryptionMeta.time) {
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
    const dbData = await decodeRisuSave(db)
    counts.characters = dbData.characters?.length ?? 0
    counts.charactersTotal = counts.characters
    counts.presets = dbData.botPresets?.length ?? 0
    report('decoding-database')

    const missingColdStorageKeys: string[] = []
    for (const key of await listColdDataKeys(dbData)) {
        if (restoredColdStorageKeys.has(key)) continue
        const existingColdStorage = await getColdStorageItem(key, { accountFallback: true })
        if (!isColdStorageBackupData(existingColdStorage)) {
            missingColdStorageKeys.push(key)
        }
    }
    if (!await confirmIncompleteColdStorageOperation(dbData, missingColdStorageKeys, 'restore')) {
        throw cancelledError()
    }
    checkCancelled()

    report('activating')
    await installLocalBackup(dbData, {
        replaceDatabase: replacePersistentDatabase,
        publishAcceptedRevision: publishCurrentOfficialRevision,
        relaunch: async () => {
            report('restarting-app')
            // Android has no process relauncher, so the WebView reloads instead.
            if (isTauriDesktop) {
                await relaunch()
            } else {
                location.search = ''
            }
        },
    })

    return { warningCodes }
}
