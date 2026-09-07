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
    const backupActionClass = 'shrink-0 rounded-md border border-darkborderc bg-darkbutton px-3 py-1 text-sm text-textcolor transition-colors hover:bg-selected disabled:opacity-50 disabled:cursor-not-allowed focus-visible:outline focus-visible:outline-2 focus-visible:outline-darkborderc focus-visible:outline-offset-2'
    const labels = {
        total: language.risuNest.storage.total,
        media: language.risuNest.storage.media,
        inlays: language.risuNest.storage.inlays,
        plugins: language.risuNest.storage.plugins,
        snapshots: language.risuNest.storage.snapshots,
        conflictBackups: language.risuNest.storage.conflictBackups,
    }
    const formatCount = (value: number): string => value.toLocaleString()
    const listSummary = (count: number, bytes: number): string => language.risuNest.storage.listSummary
        .replace('{0}', formatCount(count))
        .replace('{1}', formatRisuNestStorageBytes(bytes))

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

    async function previewGc(): Promise<void> {
        try { await dashboard.previewGc() } catch (error) { showActionError(error) }
    }

    async function executeGc(): Promise<void> {
        const preview = state.gcPreview
        if (!preview) return
        const message = language.risuNest.storage.gcConfirm
            .replace('{0}', formatCount(preview.candidateCount))
            .replace('{1}', formatRisuNestStorageBytes(preview.candidateBytes))
        if (!await alertConfirm(message)) return
        try { await dashboard.executeGc() } catch (error) { showActionError(error) }
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
            .replace('{0}', formatCount(rollup.counts.characters))
            .replace('{1}', formatCount(rollup.counts.conversations))
            .replace('{2}', formatCount(rollup.counts.messages))}
        {#if rollup.counts.trashedCharacters > 0}
            {' '}{language.risuNest.storage.trashedCount.replace('{0}', formatCount(rollup.counts.trashedCharacters))}
        {/if}
    </p>

    <details data-storage-backup-list class="mt-3">
        <summary class="cursor-pointer select-none py-1 text-textcolor">{language.risuNest.storage.snapshots}{#if state.snapshots.length > 0}{' '}<span class="text-sm text-textcolor2">({listSummary(state.snapshots.length, rollup.snapshotBytes)})</span>{/if}</summary>
        {#each state.snapshots as snapshot}
            <div data-storage-backup-row class="ml-5 flex flex-wrap items-center gap-3 py-1 text-sm"><span class="min-w-0 break-words">{new Date(snapshot.modifiedAt).toLocaleString()} ({formatRisuNestStorageBytes(snapshot.bytes)})</span><button class={backupActionClass} disabled={isBusy(`delete-snapshot:${snapshot.path}`)} onclick={() => deleteSnapshot(snapshot.path)}>{isBusy(`delete-snapshot:${snapshot.path}`) ? language.loading : language.remove}</button></div>
        {:else}
            <p class="ml-5 py-1 text-sm text-textcolor2">{language.risuNest.storage.emptyList}</p>
        {/each}
    </details>
    <details data-storage-backup-list class="mt-1">
        <summary class="cursor-pointer select-none py-1 text-textcolor">{language.risuNest.storage.conflictBackups}{#if state.conflictBackups.length > 0}{' '}<span class="text-sm text-textcolor2">({listSummary(state.conflictBackups.length, rollup.conflictBackupBytes)})</span>{/if}</summary>
        {#each state.conflictBackups as backup}
            <div data-storage-backup-row class="ml-5 flex flex-wrap items-center gap-3 py-1 text-sm"><span class="min-w-0 break-words">{new Date(backup.createdAt).toLocaleString()} ({formatRisuNestStorageBytes(backup.byteLength)})</span><button class={backupActionClass} disabled={isBusy(`delete-conflict-backup:${backup.id}`)} onclick={() => deleteConflictBackup(backup.id)}>{isBusy(`delete-conflict-backup:${backup.id}`) ? language.loading : language.remove}</button></div>
        {:else}
            <p class="ml-5 py-1 text-sm text-textcolor2">{language.risuNest.storage.emptyList}</p>
        {/each}
    </details>
    <details data-storage-backup-list class="mt-1">
        <summary class="cursor-pointer select-none py-1 text-textcolor">{language.risuNest.storage.syncBackups}{#if state.peerBackups.length > 0}{' '}<span class="text-sm text-textcolor2">({listSummary(state.peerBackups.length, rollup.peerBackupBytes)})</span>{/if}</summary>
        {#each state.peerBackups as backup}
            <div data-storage-backup-row class="ml-5 flex flex-wrap items-center gap-3 py-1 text-sm"><span class="min-w-0 break-words">{new Date(backup.modifiedAt).toLocaleString()} ({formatRisuNestStorageBytes(backup.bytes)})</span><button class={backupActionClass} data-path={backup.path} disabled={isBusy(`delete-peer-backup:${backup.path}`)} onclick={() => deletePeerBackup(backup.path)}>{isBusy(`delete-peer-backup:${backup.path}`) ? language.loading : language.remove}</button></div>
        {:else}
            <p class="ml-5 py-1 text-sm text-textcolor2">{language.risuNest.storage.emptyList}</p>
        {/each}
    </details>

    <div data-storage-action-row class="mt-4 flex flex-col gap-2">
        <div data-storage-action="snapshot" class="flex flex-wrap items-center gap-2">
            <Button disabled={isBusy('create-snapshot')} onclick={createSnapshot}>{isBusy('create-snapshot') ? language.loading : language.risuNest.storage.createSnapshot}</Button>
        </div>
        <div data-storage-action="temp" class="flex flex-wrap items-center gap-2">
            <Button disabled={isBusy('calculate-temp')} onclick={calculateTempSize}>{isBusy('calculate-temp') ? language.loading : language.risuNest.storage.calculateSize}</Button>
            <div role="status" aria-live="polite" class="flex flex-wrap items-center gap-2">
                {#if state.tempUsage}
                    <span class="text-sm text-textcolor2">{language.risuNest.storage.tempUsage.replace('{size}', formatRisuNestStorageBytes(state.tempUsage.bytes))}</span>
                {/if}
                {#if state.tempUsage || isBusy('cleanup-temp')}
                    <Button size="sm" disabled={isBusy('cleanup-temp')} onclick={cleanTemp}>{isBusy('cleanup-temp') ? language.loading : language.risuNest.storage.cleanSyncTemp}</Button>
                {/if}
            </div>
        </div>
        {#if state.tempUsage || isBusy('cleanup-temp')}
            <p class="text-sm text-textcolor2">{language.risuNest.storage.cleanSyncTempNote}</p>
        {/if}
        <div data-storage-action="gc" class="flex flex-wrap items-center gap-2">
            <Button disabled={isBusy('preview-gc') || isBusy('execute-gc')} onclick={previewGc}>{isBusy('preview-gc') ? language.loading : language.risuNest.storage.gcRun}</Button>
            <div role="status" aria-live="polite" class="flex flex-wrap items-center gap-2">
                {#if state.gcPreview}
                    <span class="text-sm text-textcolor2">{language.risuNest.storage.gcResult.replace('{0}', formatCount(state.gcPreview.candidateCount)).replace('{1}', formatRisuNestStorageBytes(state.gcPreview.candidateBytes))}</span>
                    <Button size="sm" disabled={isBusy('execute-gc')} onclick={executeGc}>{isBusy('execute-gc') ? language.loading : language.risuNest.storage.gcRunConfirm}</Button>
                {:else if state.gcResult}
                    <span class="text-sm text-textcolor2">{language.risuNest.storage.gcDeletedResult.replace('{0}', formatCount(state.gcResult.deletedCount)).replace('{1}', formatRisuNestStorageBytes(state.gcResult.deletedBytes))}</span>
                {/if}
            </div>
        </div>
    </div>
{/if}
