import { Channel, invoke } from '@tauri-apps/api/core'
import { language } from 'src/lang'
import { alertConfirm, alertError, alertNormal } from '../alert'
import { restartNativeApp } from './nativePersistentMaintenance'
import { flushPendingData } from './persistentDataRuntime.svelte'
import { formatRisuNestStorageBytes } from './risuNestStorageDashboard'

/** How a start that compacted the database ended. */
export type StoreCompactionResult =
    | { kind: 'compacted'; reclaimedBytes: number }
    | { kind: 'no-space'; shortfallBytes: number }
    | { kind: 'failed' }

type StoreCompactionRequest = { kind: 'requested' } | { kind: 'no-space'; shortfallBytes: number }

interface StoreCompactionProgress {
    doneBytes: number
    totalBytes: number
}

/**
 * Compacts the database before the store opens when the previous start asked
 * for it, and passes the loading text to `show` while it copies. Returns null
 * when nothing was requested.
 */
export async function compactRequestedStoreAtStart(show: (text: string) => void): Promise<StoreCompactionResult | null> {
    const progress = new Channel<StoreCompactionProgress>()
    progress.onmessage = ({ doneBytes, totalBytes }) => {
        // The store opens after the last file, so the figure never claims it is done.
        const percent = totalBytes > 0 ? Math.min(99, Math.floor((doneBytes / totalBytes) * 100)) : 0
        show(language.risuNest.startup.compacting.replace('{0}', String(percent)))
    }
    try {
        return await invoke<StoreCompactionResult | null>('pds_store_compaction_at_start', { progress })
    } catch (error) {
        // The store open that follows reports a store that cannot open.
        console.error('Store compaction at start failed', error)
        return null
    }
}

/** Records a compaction for the next start and restarts the app. */
export async function requestStoreCompaction(): Promise<void> {
    const strings = language.risuNest.storage
    await flushPendingData('store-compaction')
    const request = await invoke<StoreCompactionRequest>('pds_store_compaction_request')
    if (request.kind === 'no-space') {
        alertError(strings.compactNoSpace.replace('{0}', formatRisuNestStorageBytes(request.shortfallBytes)))
        return
    }
    try {
        await restartNativeApp()
    } catch (error) {
        console.error('Restart for store compaction failed', error)
        alertError(strings.compactRestartFailed)
    }
}

/** After the terms: the result of a compacting start, or else the offer. */
export async function showStoreCompactionNotice(result: StoreCompactionResult | null): Promise<void> {
    const strings = language.risuNest.storage
    if (result) {
        if (result.kind === 'compacted') alertNormal(strings.compacted.replace('{0}', formatRisuNestStorageBytes(result.reclaimedBytes)))
        else if (result.kind === 'no-space') alertError(strings.compactStartNoSpace.replace('{0}', formatRisuNestStorageBytes(result.shortfallBytes)))
        else alertError(strings.compactFailed)
        return
    }
    const reclaimable = await invoke<number | null>('pds_store_compaction_offer')
    if (reclaimable === null || !await alertConfirm(strings.compactOffer.replace('{0}', formatRisuNestStorageBytes(reclaimable)))) return
    try {
        await requestStoreCompaction()
    } catch (error) {
        console.error('Store compaction request failed', error)
        alertError(strings.compactFailed)
    }
}
