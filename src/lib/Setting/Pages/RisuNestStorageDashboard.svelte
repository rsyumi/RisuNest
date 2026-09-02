<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError } from 'src/ts/alert'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import {
        cleanupPeerTemp,
        createNativePersistentSnapshot,
        deleteNativePersistentSnapshot,
        executeNativePersistentAssetGc,
        getNativePersistentStorageStats,
        getPeerTempUsage,
        isNativePeerBackupDeleteError,
        listNativePersistentSnapshots,
        listPeerBackups,
        previewNativePersistentAssetGc,
        removePeerBackup,
    } from 'src/ts/storage/nativePersistentMaintenance'
    import { getSyncConflictBackupStore } from 'src/ts/storage/sync/syncConflictBackup'
    import {
        createRisuNestStorageDashboard,
        formatRisuNestStorageBytes,
        storageDashboardRollup,
    } from 'src/ts/storage/risuNestStorageDashboard'

    const conflictStore = getSyncConflictBackupStore()
    const dashboard = createRisuNestStorageDashboard({
        getStats: getNativePersistentStorageStats,
        listSnapshots: listNativePersistentSnapshots,
        listConflictBackups: () => conflictStore.list(),
        listPeerBackups,
        getTemp: getPeerTempUsage,
        cleanupTemp: cleanupPeerTemp,
        previewGc: previewNativePersistentAssetGc,
        executeGc: executeNativePersistentAssetGc,
        deleteSnapshot: deleteNativePersistentSnapshot,
        deleteConflictBackup: (id) => conflictStore.remove(id),
        deletePeerBackup: removePeerBackup,
        createSnapshot: createNativePersistentSnapshot,
    })
    let state = $state(dashboard.snapshot())
    let rollup = $derived(state.stats
        ? storageDashboardRollup(state.stats, state.snapshots, state.conflictBackups, state.peerBackups)
        : null)
    const labels = {
        total: language.risuNest.storage.total,
        media: language.risuNest.storage.media,
        inlays: language.risuNest.storage.inlays,
        plugins: language.risuNest.storage.plugins,
        snapshots: language.risuNest.storage.snapshots,
        conflictBackups: language.risuNest.storage.conflictBackups,
    }

    function showActionError(error: unknown, fallback = language.risuNest.storage.actionFailed): void {
        alertError(isNativePeerBackupDeleteError(error)?.code === 'peer-backup-in-use'
            ? language.risuNest.storage.syncBackupInUse
            : fallback)
    }

    function isBusy(action: string): boolean {
        return state.busy.includes(action)
    }

    async function calculateTempSize(): Promise<void> {
        try { await dashboard.calculateTempSize() } catch (error) { showActionError(error, language.risuNest.storage.calculateSizeFailed) }
    }

    async function cleanTemp(): Promise<void> {
        if (!await alertConfirm(language.risuNest.storage.cleanSyncTemp)) return
        try { await dashboard.cleanupTemp() } catch (error) { showActionError(error) }
    }

    async function runGc(): Promise<void> {
        try {
            if (!state.gcPreview) {
                await dashboard.previewGc()
                return
            }
            const preview = state.gcPreview
            const message = language.risuNest.storage.gcConfirm
                .replace('{0}', String(preview.candidateCount))
                .replace('{1}', formatRisuNestStorageBytes(preview.candidateBytes))
            if (!await alertConfirm(message)) return
            await dashboard.executeGc()
        } catch (error) { showActionError(error) }
    }

    async function deleteSnapshot(path: string): Promise<void> {
        if (!await alertConfirm(language.risuNest.storage.deleteSnapshotConfirm)) return
        try { await dashboard.deleteSnapshot(path) } catch (error) { showActionError(error) }
    }

    async function deleteConflictBackup(id: string): Promise<void> {
        if (!await alertConfirm(language.risuNest.storage.deleteConflictBackupConfirm)) return
        try { await dashboard.deleteConflictBackup(id) } catch (error) { showActionError(error) }
    }

    async function deletePeerBackup(path: string): Promise<void> {
        if (!await alertConfirm(language.risuNest.storage.deleteSyncBackupConfirm)) return
        try { await dashboard.deletePeerBackup(path) } catch (error) { showActionError(error) }
    }

    async function createSnapshot(): Promise<void> {
        try { await dashboard.createSnapshot() } catch (error) { showActionError(error) }
    }

    const unsubscribe = dashboard.subscribe((next) => { state = next })
    onMount(() => { void dashboard.load() })
    onDestroy(unsubscribe)
</script>

<h2 class="mb-2 text-2xl font-bold mt-6">{language.risuNest.storage.title}</h2>
{#if state.loadFailed}
    <div class="text-textcolor2" role="alert" aria-live="assertive">
        <span>{rollup ? language.risuNest.storage.staleTotals : language.risuNest.storage.loadFailed}</span>
        <Button size="sm" disabled={state.loading} onclick={() => dashboard.load()}>{state.loading ? language.loading : language.risuNest.storage.retry}</Button>
    </div>
{/if}
{#if state.loading && !rollup}
    <div class="grid grid-cols-2 sm:grid-cols-3 gap-2" role="status" aria-live="polite" aria-label={language.loading}>
        {#each Array(6) as _}
            <div data-storage-card-placeholder class="h-[68px] animate-pulse rounded-md bg-darkbutton" aria-hidden="true"></div>
        {/each}
    </div>
{:else if rollup}
    <div class="grid grid-cols-2 sm:grid-cols-3 gap-2">
        {#each rollup.cards as card}
            <div class="rounded-md bg-darkbg p-3 text-textcolor">
                <div class="text-sm text-textcolor2">{labels[card.id]}</div>
                <div class="font-semibold">{formatRisuNestStorageBytes(card.bytes)}</div>
            </div>
        {/each}
    </div>
    <p class="mt-2 text-sm text-textcolor2">
        {language.risuNest.storage.counts
            .replace('{0}', String(rollup.counts.characters))
            .replace('{1}', String(rollup.counts.conversations))
            .replace('{2}', String(rollup.counts.messages))}
        {#if rollup.counts.trashedCharacters > 0}
            {' '}{language.risuNest.storage.trashedCount.replace('{0}', String(rollup.counts.trashedCharacters))}
        {/if}
    </p>

    <details data-storage-backup-list class="mt-3">
        <summary>{language.risuNest.storage.snapshots}</summary>
        {#each state.snapshots as snapshot}
            <div class="flex items-center justify-between gap-2 py-1 text-sm"><span>{new Date(snapshot.modifiedAt).toLocaleString()} ({formatRisuNestStorageBytes(snapshot.bytes)})</span><button disabled={isBusy(`delete-snapshot:${snapshot.path}`)} onclick={() => deleteSnapshot(snapshot.path)}>{isBusy(`delete-snapshot:${snapshot.path}`) ? language.loading : language.remove}</button></div>
        {/each}
    </details>
    <details data-storage-backup-list class="mt-2">
        <summary>{language.risuNest.storage.conflictBackups}</summary>
        {#each state.conflictBackups as backup}
            <div class="flex items-center justify-between gap-2 py-1 text-sm"><span>{new Date(backup.createdAt).toLocaleString()} ({formatRisuNestStorageBytes(backup.byteLength)})</span><button disabled={isBusy(`delete-conflict-backup:${backup.id}`)} onclick={() => deleteConflictBackup(backup.id)}>{isBusy(`delete-conflict-backup:${backup.id}`) ? language.loading : language.remove}</button></div>
        {/each}
    </details>
    <details data-storage-backup-list class="mt-2">
        <summary>{language.risuNest.storage.syncBackups}</summary>
        {#each state.peerBackups as backup}
            <div class="flex items-center justify-between gap-2 py-1 text-sm"><span>{new Date(backup.modifiedAt).toLocaleString()} ({formatRisuNestStorageBytes(backup.bytes)})</span><button data-path={backup.path} disabled={isBusy(`delete-peer-backup:${backup.path}`)} onclick={() => deletePeerBackup(backup.path)}>{isBusy(`delete-peer-backup:${backup.path}`) ? language.loading : language.remove}</button></div>
        {/each}
    </details>

    <div data-storage-action-row class="mt-3 flex flex-wrap gap-2">
        <Button disabled={isBusy('create-snapshot')} onclick={createSnapshot}>{isBusy('create-snapshot') ? language.loading : language.risuNest.storage.createSnapshot}</Button>
        {#if state.tempUsage || isBusy('cleanup-temp')}
            <Button disabled={isBusy('cleanup-temp')} onclick={cleanTemp}>{isBusy('cleanup-temp') ? language.loading : language.risuNest.storage.cleanSyncTemp}</Button>
        {:else}
            <Button disabled={isBusy('calculate-temp')} onclick={calculateTempSize}>{isBusy('calculate-temp') ? language.loading : language.risuNest.storage.calculateSize}</Button>
        {/if}
        <Button disabled={isBusy('preview-gc') || isBusy('execute-gc')} onclick={runGc}>{isBusy('preview-gc') || isBusy('execute-gc') ? language.loading : language.risuNest.storage.gcRun}</Button>
    </div>
    <div role="status" aria-live="polite">
        <p class="mt-1 text-sm text-textcolor2">{language.risuNest.storage.cleanSyncTempNote}</p>
        {#if state.tempUsage}
            <p class="mt-1 text-sm text-textcolor2">{language.risuNest.storage.tempUsage.replace('{size}', formatRisuNestStorageBytes(state.tempUsage.bytes))}</p>
        {/if}
        {#if state.gcPreview}
            <p class="mt-1 text-sm text-textcolor2">{language.risuNest.storage.gcResult.replace('{0}', String(state.gcPreview.candidateCount)).replace('{1}', formatRisuNestStorageBytes(state.gcPreview.candidateBytes))}</p>
        {:else if state.gcResult}
            <p class="mt-1 text-sm text-textcolor2">{language.risuNest.storage.gcDeletedResult.replace('{0}', String(state.gcResult.deletedCount)).replace('{1}', formatRisuNestStorageBytes(state.gcResult.deletedBytes))}</p>
        {/if}
    </div>
{/if}
