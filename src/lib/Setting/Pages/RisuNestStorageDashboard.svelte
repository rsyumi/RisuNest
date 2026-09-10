<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { ChevronRight } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError } from 'src/ts/alert'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
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
        type RisuNestStorageCardId,
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
    const strings = language.risuNest.storage
    const formatCount = (value: number): string => value.toLocaleString()
    const listSummary = (count: number, bytes: number): string => strings.listSummary
        .replace('{0}', formatCount(count))
        .replace('{1}', formatRisuNestStorageBytes(bytes))
    const cardBytes = (id: RisuNestStorageCardId): number => rollup?.cards.find((card) => card.id === id)?.bytes ?? 0
    let totalBytes = $derived(cardBytes('total'))
    // The bar partitions the total: media, database, and the three backup kinds.
    let segments = $derived(rollup && state.stats ? [
        { id: 'media', label: strings.media, bytes: cardBytes('media'), color: 'bg-borderc' },
        { id: 'database', label: strings.database, bytes: state.stats.databaseBytes, color: 'bg-secondary-400' },
        { id: 'syncBackups', label: strings.syncBackups, bytes: rollup.peerBackupBytes, color: 'bg-primary-300' },
        { id: 'snapshots', label: strings.snapshots, bytes: rollup.snapshotBytes, color: 'bg-success-400' },
        { id: 'conflictBackups', label: strings.conflictBackups, bytes: rollup.conflictBackupBytes, color: 'bg-danger-400' },
    ] : [])
    const listHeaderClass = 'flex cursor-pointer list-none items-center gap-2 px-4 py-2.5 text-[15px] select-none [&::-webkit-details-marker]:hidden'
    const listRowClass = 'flex items-center gap-3 border-t border-darkborderc/55 py-1.5 pr-4 pl-10 text-sm'
    const listEmptyClass = 'border-t border-darkborderc/55 py-2 pr-4 pl-10 text-sm text-textcolor2'

    function showActionError(error: unknown, fallback = strings.actionFailed): void {
        alertError(isNativePeerBackupDeleteError(error)?.code === 'peer-backup-in-use'
            ? strings.syncBackupInUse
            : fallback)
    }

    function isBusy(action: string): boolean {
        return state.busy.includes(action)
    }

    async function calculateTempSize(): Promise<void> {
        try { await dashboard.calculateTempSize() } catch (error) { showActionError(error, strings.calculateSizeFailed) }
    }

    async function cleanTemp(): Promise<void> {
        if (!await alertConfirm(strings.cleanSyncTempConfirm)) return
        try { await dashboard.cleanupTemp() } catch (error) { showActionError(error) }
    }

    async function previewGc(): Promise<void> {
        try { await dashboard.previewGc() } catch (error) { showActionError(error) }
    }

    async function executeGc(): Promise<void> {
        const preview = state.gcPreview
        if (!preview) return
        const message = strings.gcConfirm
            .replace('{0}', formatCount(preview.candidateCount))
            .replace('{1}', formatRisuNestStorageBytes(preview.candidateBytes))
        if (!await alertConfirm(message)) return
        try { await dashboard.executeGc() } catch (error) { showActionError(error) }
    }

    async function deleteSnapshot(path: string): Promise<void> {
        if (!await alertConfirm(strings.deleteSnapshotConfirm)) return
        try { await dashboard.deleteSnapshot(path) } catch (error) { showActionError(error) }
    }

    async function deleteConflictBackup(id: string): Promise<void> {
        if (!await alertConfirm(strings.deleteConflictBackupConfirm)) return
        try { await dashboard.deleteConflictBackup(id) } catch (error) { showActionError(error) }
    }

    async function deletePeerBackup(path: string): Promise<void> {
        if (!await alertConfirm(strings.deleteSyncBackupConfirm)) return
        try { await dashboard.deletePeerBackup(path) } catch (error) { showActionError(error) }
    }

    async function createSnapshot(): Promise<void> {
        try { await dashboard.createSnapshot() } catch (error) { showActionError(error) }
    }

    const unsubscribe = dashboard.subscribe((next) => { state = next })
    onMount(() => { void dashboard.load() })
    onDestroy(unsubscribe)
</script>

<SettingGroup id="risunest-storage" title={strings.title}>
    {#snippet actions()}
        <Button size="sm" styled="outlined" disabled={state.loading} onclick={() => dashboard.load()}>{state.loading ? language.loading : strings.refresh}</Button>
    {/snippet}
    {#if state.loadFailed}
        <div class="flex flex-wrap items-center gap-2 px-4 py-3 text-sm text-textcolor2" role="alert" aria-live="assertive">
            <span>{rollup ? strings.staleTotals : strings.loadFailed}</span>
            <Button size="sm" disabled={state.loading} onclick={() => dashboard.load()}>{state.loading ? language.loading : strings.retry}</Button>
        </div>
    {/if}
    {#if state.loading && !rollup}
        <div class="grid grid-cols-2 gap-2 p-4 sm:grid-cols-3" role="status" aria-live="polite" aria-label={language.loading}>
            {#each Array(6) as _}
                <div data-storage-card-placeholder class="h-[68px] animate-pulse rounded-md bg-darkbutton" aria-hidden="true"></div>
            {/each}
        </div>
    {:else if rollup}
        <div data-storage-summary class="p-4">
            <div class="flex flex-wrap items-end justify-between gap-x-6 gap-y-2">
                <div>
                    <div class="text-xs text-textcolor2">{strings.total}</div>
                    <div class="text-[1.7rem] leading-tight font-bold tabular-nums">{formatRisuNestStorageBytes(totalBytes)}</div>
                </div>
                <p class="text-sm text-textcolor2">
                    {strings.counts
                        .replace('{0}', formatCount(rollup.counts.characters))
                        .replace('{1}', formatCount(rollup.counts.conversations))
                        .replace('{2}', formatCount(rollup.counts.messages))}
                    {#if rollup.counts.trashedCharacters > 0}
                        {' '}{strings.trashedCount.replace('{0}', formatCount(rollup.counts.trashedCharacters))}
                    {/if}
                </p>
            </div>
            <div class="mt-3 flex h-3 gap-0.5 overflow-hidden rounded-full bg-bgcolor" aria-hidden="true">
                {#each segments as segment (segment.id)}
                    {#if segment.bytes > 0}
                        <div class="h-full {segment.color}" style:width={`${(segment.bytes / Math.max(1, totalBytes)) * 100}%`}></div>
                    {/if}
                {/each}
            </div>
            <ul data-storage-legend class="mt-3 grid grid-cols-[repeat(auto-fit,minmax(150px,1fr))] gap-x-5 gap-y-1.5 text-sm">
                {#each segments as segment (segment.id)}
                    <li class="flex items-center gap-2">
                        <span class="h-2.5 w-2.5 shrink-0 rounded-xs {segment.color}" aria-hidden="true"></span>
                        <span class="flex-1 text-textcolor2">{segment.label}</span>
                        <span class="tabular-nums">{formatRisuNestStorageBytes(segment.bytes)}</span>
                    </li>
                {/each}
            </ul>
            <p class="mt-2 text-xs text-textcolor2">
                {strings.subMetrics
                    .replace('{0}', formatRisuNestStorageBytes(cardBytes('inlays')))
                    .replace('{1}', formatRisuNestStorageBytes(cardBytes('plugins')))}
            </p>
        </div>

        <details data-storage-backup-list class="group">
            <summary class={listHeaderClass}><ChevronRight size={16} class="shrink-0 text-textcolor2 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" /><span>{strings.snapshots}</span>{#if state.snapshots.length > 0}{' '}<span class="ml-auto text-sm text-textcolor2 tabular-nums">{listSummary(state.snapshots.length, rollup.snapshotBytes)}</span>{/if}</summary>
            {#each state.snapshots as snapshot}
                <div data-storage-backup-row class={listRowClass}><span class="min-w-0 flex-1 break-words tabular-nums">{new Date(snapshot.modifiedAt).toLocaleString()}</span><span class="text-textcolor2 tabular-nums">{formatRisuNestStorageBytes(snapshot.bytes)}</span><Button size="sm" styled="outlined" disabled={isBusy(`delete-snapshot:${snapshot.path}`)} onclick={() => deleteSnapshot(snapshot.path)}>{isBusy(`delete-snapshot:${snapshot.path}`) ? language.loading : language.remove}</Button></div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/each}
        </details>
        <details data-storage-backup-list class="group">
            <summary class={listHeaderClass}><ChevronRight size={16} class="shrink-0 text-textcolor2 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" /><span>{strings.conflictBackups}</span>{#if state.conflictBackups.length > 0}{' '}<span class="ml-auto text-sm text-textcolor2 tabular-nums">{listSummary(state.conflictBackups.length, rollup.conflictBackupBytes)}</span>{/if}</summary>
            {#each state.conflictBackups as backup}
                <div data-storage-backup-row class={listRowClass}><span class="min-w-0 flex-1 break-words tabular-nums">{new Date(backup.createdAt).toLocaleString()}</span><span class="text-textcolor2 tabular-nums">{formatRisuNestStorageBytes(backup.byteLength)}</span><Button size="sm" styled="outlined" disabled={isBusy(`delete-conflict-backup:${backup.id}`)} onclick={() => deleteConflictBackup(backup.id)}>{isBusy(`delete-conflict-backup:${backup.id}`) ? language.loading : language.remove}</Button></div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/each}
        </details>
        <details data-storage-backup-list class="group">
            <summary class={listHeaderClass}><ChevronRight size={16} class="shrink-0 text-textcolor2 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" /><span>{strings.syncBackups}</span>{#if state.peerBackups.length > 0}{' '}<span class="ml-auto text-sm text-textcolor2 tabular-nums">{listSummary(state.peerBackups.length, rollup.peerBackupBytes)}</span>{/if}</summary>
            {#each state.peerBackups as backup}
                <div data-storage-backup-row class={listRowClass}><span class="min-w-0 flex-1 break-words tabular-nums">{new Date(backup.modifiedAt).toLocaleString()}</span><span class="text-textcolor2 tabular-nums">{formatRisuNestStorageBytes(backup.bytes)}</span><button class="shrink-0 rounded-md border border-darkborderc bg-transparent px-2 py-1 text-sm text-textcolor2 transition-colors duration-200 hover:bg-selected hover:text-textcolor focus:outline-hidden focus:ring-2 focus:ring-selected disabled:cursor-not-allowed disabled:opacity-50" data-path={backup.path} disabled={isBusy(`delete-peer-backup:${backup.path}`)} onclick={() => deletePeerBackup(backup.path)}>{isBusy(`delete-peer-backup:${backup.path}`) ? language.loading : language.remove}</button></div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/each}
        </details>

        <div data-storage-action-row class="divide-y divide-darkborderc/55">
            <SettingRow data-storage-action="snapshot" label={strings.createSnapshotTitle} help={strings.createSnapshotHelp}>
                <Button disabled={isBusy('create-snapshot')} onclick={createSnapshot}>{isBusy('create-snapshot') ? language.loading : strings.createSnapshot}</Button>
            </SettingRow>
            <SettingRow data-storage-action="temp" label={strings.tempTitle} help={strings.cleanSyncTempNote}>
                {#snippet below()}
                    <div role="status" aria-live="polite">
                        {#if state.tempUsage}
                            <p class="mt-1 text-sm tabular-nums">{strings.tempUsage.replace('{size}', formatRisuNestStorageBytes(state.tempUsage.bytes))}</p>
                        {/if}
                    </div>
                {/snippet}
                <Button styled="outlined" disabled={isBusy('calculate-temp')} onclick={calculateTempSize}>{isBusy('calculate-temp') ? language.loading : strings.calculateSize}</Button>
                {#if state.tempUsage || isBusy('cleanup-temp')}
                    <Button disabled={isBusy('cleanup-temp')} onclick={cleanTemp}>{isBusy('cleanup-temp') ? language.loading : strings.cleanSyncTemp}</Button>
                {/if}
            </SettingRow>
            <SettingRow data-storage-action="gc" label={strings.gcTitle} help={strings.gcHelp}>
                {#snippet below()}
                    <div role="status" aria-live="polite">
                        {#if state.gcPreview}
                            <p class="mt-1 text-sm tabular-nums">{strings.gcResult.replace('{0}', formatCount(state.gcPreview.candidateCount)).replace('{1}', formatRisuNestStorageBytes(state.gcPreview.candidateBytes))}</p>
                        {:else if state.gcResult}
                            <p class="mt-1 text-sm tabular-nums">{strings.gcDeletedResult.replace('{0}', formatCount(state.gcResult.deletedCount)).replace('{1}', formatRisuNestStorageBytes(state.gcResult.deletedBytes))}</p>
                        {/if}
                    </div>
                {/snippet}
                <Button styled="outlined" disabled={isBusy('preview-gc') || isBusy('execute-gc')} onclick={previewGc}>{isBusy('preview-gc') ? language.loading : strings.gcRun}</Button>
                {#if state.gcPreview}
                    <Button disabled={isBusy('execute-gc')} onclick={executeGc}>{isBusy('execute-gc') ? language.loading : strings.gcRunConfirm}</Button>
                {/if}
            </SettingRow>
        </div>
    {/if}
</SettingGroup>
