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

    function isInUseBackupError(error: unknown): boolean {
        return typeof error === 'object' && error !== null
            && 'code' in error && (error as { code?: unknown }).code === 'peer-backup-in-use'
    }

    function showActionError(error: unknown): void {
        alertError(isInUseBackupError(error)
            ? language.risuNest.storage.syncBackupInUse
            : language.risuNest.storage.actionFailed)
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
        if (!await alertConfirm(language.risuNest.storage.deleteSnapshotConfirm)) return
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
{#if state.loading && !rollup}
    <span class="text-textcolor2">Loading...</span>
{:else if state.loadFailed && !rollup}
    <div class="text-textcolor2">
        <span>{language.risuNest.storage.loadFailed}</span>
        <Button size="sm" onclick={() => dashboard.load()}>{language.risuNest.storage.retry}</Button>
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

    <div class="mt-3 flex flex-wrap gap-2">
        <Button disabled={state.busy !== null} onclick={createSnapshot}>{language.risuNest.storage.createSnapshot}</Button>
        <Button disabled={state.busy !== null} onclick={() => dashboard.calculateTempSize()}>{language.risuNest.storage.calculateSize}</Button>
        <Button disabled={state.busy !== null} onclick={cleanTemp}>{language.risuNest.storage.cleanSyncTemp}</Button>
        <Button disabled={state.busy !== null} onclick={runGc}>{language.risuNest.storage.gcRun}</Button>
    </div>
    <p class="mt-1 text-sm text-textcolor2">{language.risuNest.storage.cleanSyncTempNote}{#if state.tempUsage} {formatRisuNestStorageBytes(state.tempUsage.bytes)}{/if}</p>
    {#if state.gcPreview}
        <p class="mt-1 text-sm text-textcolor2">{language.risuNest.storage.gcResult.replace('{0}', String(state.gcPreview.candidateCount)).replace('{1}', formatRisuNestStorageBytes(state.gcPreview.candidateBytes))}</p>
    {/if}

    <details class="mt-3">
        <summary>{language.risuNest.storage.snapshots}</summary>
        {#each state.snapshots as snapshot}
            <div class="flex items-center justify-between gap-2 py-1 text-sm"><span>{snapshot.path} ({formatRisuNestStorageBytes(snapshot.bytes)})</span><button disabled={state.busy !== null} onclick={() => deleteSnapshot(snapshot.path)}>{language.remove}</button></div>
        {/each}
    </details>
    <details class="mt-2">
        <summary>{language.risuNest.storage.conflictBackups}</summary>
        {#each state.conflictBackups as backup}
            <div class="flex items-center justify-between gap-2 py-1 text-sm"><span>{new Date(backup.createdAt).toLocaleString()} ({formatRisuNestStorageBytes(backup.byteLength)})</span><button disabled={state.busy !== null} onclick={() => deleteConflictBackup(backup.id)}>{language.remove}</button></div>
        {/each}
    </details>
    <details class="mt-2">
        <summary>{language.risuNest.storage.syncBackups}</summary>
        {#each state.peerBackups as backup}
            <div class="flex items-center justify-between gap-2 py-1 text-sm"><span>{backup.path} ({formatRisuNestStorageBytes(backup.bytes)})</span><button data-path={backup.path} disabled={state.busy !== null} onclick={() => deletePeerBackup(backup.path)}>{language.remove}</button></div>
        {/each}
    </details>
{/if}
