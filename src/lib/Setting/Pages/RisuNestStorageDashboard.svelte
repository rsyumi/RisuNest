<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { ChevronRight } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertNormal } from 'src/ts/alert'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import SettingProgress from '../RisuNest/SettingProgress.svelte'
    import {
        getServerSyncBackupInventory,
        getServerSyncCacheUsage,
        cleanupServerSyncCache,
        deleteServerSyncBackup,
        exportServerSyncBackup,
        restoreServerSyncBackup,
    } from 'src/ts/storage/sync/serverSyncProduction'
    import {
        createNativePersistentSnapshot,
        deleteNativePersistentSnapshot,
        executeNativePersistentAssetGc,
        getNativePersistentStorageStats,
        listNativePersistentSnapshots,
        previewNativePersistentAssetGc,
        restoreNativePersistentSnapshot,
        restartNativeApp,
    } from 'src/ts/storage/nativePersistentMaintenance'
    import { getSyncConflictBackupStore } from 'src/ts/storage/sync/syncConflictBackup'
    import { openDataHealthScreen } from 'src/ts/storage/dataHealthNavigation'
    import type { NativeAssetGcCandidate } from 'src/ts/storage/nativePersistentMaintenance'
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
        getServerBackups: getServerSyncBackupInventory,
        getTemp: getServerSyncCacheUsage,
        cleanupTemp: cleanupServerSyncCache,
        previewGc: previewNativePersistentAssetGc,
        executeGc: executeNativePersistentAssetGc,
        deleteSnapshot: deleteNativePersistentSnapshot,
        deleteConflictBackup: (id) => conflictStore.remove(id),
        deleteServerBackup: deleteServerSyncBackup,
        exportServerBackup: exportServerSyncBackup,
        restoreServerBackup: restoreServerSyncBackup,
        createSnapshot: createNativePersistentSnapshot,
    })
    let view = $state(dashboard.snapshot())
    // Restoring restarts the app, so the flag only ever clears on cancel or failure.
    let restoringSnapshot: string | null = $state(null)
    let rollup = $derived(
        view.stats
            ? storageDashboardRollup(
                  view.stats,
                  view.snapshots,
                  view.conflictBackups,
                  view.serverBackups,
                  view.tempUsage,
              )
            : null,
    )
    const strings = language.risuNest.storage
    const syncText = language.risuNest.serverSync
    const syncLabels = syncText.management
    const formatCount = (value: number): string => value.toLocaleString()
    const listSummary = (count: number, bytes: number): string => strings.listSummary
        .replace('{0}', formatCount(count))
        .replace('{1}', formatRisuNestStorageBytes(bytes))
    const cardBytes = (id: RisuNestStorageCardId): number => rollup?.cards.find((card) => card.id === id)?.bytes ?? 0
    let totalBytes = $derived(cardBytes('total'))
    let serverBackupCount = $derived(
        (view.serverBackups?.completeCount ?? 0) + (view.serverBackups?.incompleteCount ?? 0),
    )
    // The bar partitions the total: media, database, and the three backup kinds.
    let segments = $derived(
        rollup && view.stats
            ? [
                  {
                      id: 'media',
                      label: strings.media,
                      bytes: cardBytes('media'),
                      color: 'bg-borderc',
                  },
                  {
                      id: 'database',
                      label: strings.database,
                      bytes: view.stats.databaseBytes,
                      color: 'bg-secondary-400',
                  },
                  {
                      id: 'syncBackups',
                      label: strings.syncBackups,
                      bytes: rollup.serverBackupBytes,
                      color: 'bg-primary-300',
                  },
                  {
                      id: 'cache',
                      label: syncLabels.cache,
                      bytes: rollup.cacheBytes,
                      color: 'bg-borderc',
                  },
                  {
                      id: 'snapshots',
                      label: strings.snapshots,
                      bytes: rollup.snapshotBytes,
                      color: 'bg-success-400',
                  },
                  {
                      id: 'conflictBackups',
                      label: strings.conflictBackups,
                      bytes: rollup.conflictBackupBytes,
                      color: 'bg-danger-400',
                  },
              ]
            : [],
    )
    const listHeaderClass = 'flex cursor-pointer list-none items-center gap-2 px-4 py-2.5 text-[15px] select-none [&::-webkit-details-marker]:hidden'
    const listRowClass = 'flex flex-wrap items-center gap-x-3 gap-y-1.5 border-t border-darkborderc/55 py-1.5 pr-4 pl-10 text-sm'
    const listNoteClass = 'px-4 py-2 text-sm text-textcolor2'
    const listEmptyClass = 'border-t border-darkborderc/55 py-2 pr-4 pl-10 text-sm text-textcolor2'

    function snapshotReason(reason: string): string {
        const reasons = strings.snapshotReasons
        if (reason === 'manual') return reasons.manual
        if (reason === 'periodic') return reasons.periodic
        if (reason === 'pre-restore') return reasons.preRestore
        return reason
    }

    function showActionError(error: unknown, fallback = strings.actionFailed): void {
        void error
        alertError(fallback)
    }

    // Creating a snapshot or reading an image fails on the same damage the data check names.
    async function showStorageFailure(error: unknown): Promise<void> {
        void error
        if (await alertConfirm(`${strings.actionFailed} ${language.risuNest.dataHealth.openResult}`))
            openDataHealthScreen()
    }

    function isBusy(action: string): boolean {
        return view.busy.includes(action)
    }

    let serverBackupBusy = $derived(view.busy.some((action) =>
        action.startsWith('restore-server-backup:') ||
        action.startsWith('export-server-backup:') ||
        action.startsWith('delete-server-backup:')))

    const gcStates: Record<NativeAssetGcCandidate['state'], string> = {
        deletable: strings.gcStateDeletable,
        recent: strings.gcStateRecent,
        held: strings.gcStateHeld,
    }
    const gcHolders: Record<string, string> = {
        snapshot: strings.gcHeldSnapshot,
        repair: strings.gcHeldRepair,
        remote: strings.gcHeldRemote,
        migration: strings.gcHeldMigration,
        job: strings.gcHeldJob,
    }
    // A kept file with no named holder is one the library itself still uses.
    const gcReason = (candidate: NativeAssetGcCandidate): string =>
        candidate.state === 'held'
            ? [
                  gcStates.held,
                  candidate.holders.length > 0
                      ? candidate.holders
                            .map((holder) => gcHolders[holder] ?? holder)
                            .join(', ')
                      : strings.gcHeldLibrary,
              ].join(' · ')
            : gcStates[candidate.state]

    // Only the files the cleanup will delete are listed; the kept ones stay behind a fold.
    let gcDeletable = $derived(view.gcPreview?.candidates?.filter((candidate) => candidate.state === 'deletable') ?? [])
    let gcKept = $derived(view.gcPreview?.candidates?.filter((candidate) => candidate.state !== 'deletable') ?? [])

    async function previewGc(): Promise<void> {
        try { await dashboard.previewGc() } catch (error) { await showStorageFailure(error) }
    }

    async function executeGc(): Promise<void> {
        const preview = view.gcPreview
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

    async function restoreSnapshot(id: string): Promise<void> {
        if (restoringSnapshot) return
        restoringSnapshot = id
        let listed = true
        try {
            await restoreNativePersistentSnapshot({
                choose: async (snapshots) => {
                    listed = snapshots.some((snapshot) => snapshot.id === id)
                    return listed ? id : null
                },
                confirm: () => alertConfirm(language.restoreLocalSnapshotConfirm),
                restart: restartNativeApp,
                onEmpty: () => { listed = false },
            })
        } catch (error) {
            showActionError(error)
        } finally {
            restoringSnapshot = null
        }
        // The snapshot disappeared since the list was loaded: show the current one.
        if (!listed) void dashboard.load()
    }

    async function deleteConflictBackup(id: string): Promise<void> {
        if (!await alertConfirm(strings.deleteConflictBackupConfirm)) return
        try { await dashboard.deleteConflictBackup(id) } catch (error) { showActionError(error) }
    }

    async function restoreServerBackup(id: string, side: 'local' | 'remote'): Promise<void> {
        if (!await alertConfirm(syncLabels.restoreConfirm)) return
        try { await dashboard.restoreServerBackup(id, side) } catch (error) { showActionError(error) }
    }

    async function deleteServerBackup(id: string): Promise<void> {
        if (!await alertConfirm(syncLabels.deleteConfirm)) return
        try {
            const result = await dashboard.deleteServerBackup(id)
            if (result.cleanup === 'pending') alertNormal(syncLabels.deleteCleanupPending)
        } catch (error) { showActionError(error) }
    }

    async function exportServerBackup(id: string, side: 'local' | 'remote'): Promise<void> {
        try { await dashboard.exportServerBackup(id, side) } catch (error) { showActionError(error) }
    }

    async function loadMoreServerBackups(): Promise<void> {
        try { await dashboard.loadMoreServerBackups() } catch (error) { showActionError(error) }
    }

    async function cleanupTemp(): Promise<void> {
        if (!await alertConfirm(syncLabels.cleanConfirm)) return
        try { await dashboard.cleanupTemp() } catch (error) { showActionError(error) }
    }

    async function createSnapshot(): Promise<void> {
        try { await dashboard.createSnapshot() } catch (error) { await showStorageFailure(error) }
    }

    const unsubscribe = dashboard.subscribe((next) => { view = next })
    onMount(() => { void dashboard.load() })
    onDestroy(unsubscribe)
</script>

<SettingGroup id="risunest-storage" title={strings.title}>
    {#snippet actions()}
        <SettingButton variant="secondary" busy={view.loading} onclick={() => dashboard.load()}>{strings.refresh}</SettingButton>
    {/snippet}
    {#if view.loadFailed}
        <div class="flex flex-wrap items-center gap-2 px-4 py-3 text-sm text-textcolor2" role="alert" aria-live="assertive">
            <span>{rollup ? strings.staleTotals : strings.loadFailed}</span>
            <SettingButton busy={view.loading} onclick={() => dashboard.load()}>{strings.retry}</SettingButton>
        </div>
    {/if}
    {#if view.loading && !rollup}
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

        {#snippet listHeader(label: string, summary: string)}
            <summary class={listHeaderClass}>
                <ChevronRight size={16} class="shrink-0 text-textcolor2 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" />
                <span>{label}</span>
                {#if summary}{' '}<span class="ml-auto text-sm text-textcolor2 tabular-nums">{summary}</span>{/if}
            </summary>
        {/snippet}
        <details data-storage-backup-list="snapshots" class="group">
            {@render listHeader(strings.snapshots, view.snapshots.length > 0 ? listSummary(view.snapshots.length, rollup.snapshotBytes) : '')}
            <p class={listNoteClass}>{strings.snapshotSizeNote}</p>
            {#each view.snapshots as snapshot (snapshot.id)}
                <div data-storage-backup-row class={listRowClass}>
                    <span class="min-w-0 flex-1 break-words tabular-nums">{new Date(snapshot.modifiedAt).toLocaleString()}<span class="ml-2 text-textcolor2">{snapshotReason(snapshot.reason)}</span></span>
                    <span class="text-textcolor2 tabular-nums">{formatRisuNestStorageBytes(snapshot.bytes)}</span>
                    <SettingButton variant="secondary" busy={restoringSnapshot === snapshot.id} disabled={restoringSnapshot !== null || isBusy(`delete-snapshot:${snapshot.id}`)} onclick={() => restoreSnapshot(snapshot.id)}>{strings.restoreSnapshot}</SettingButton>
                    <SettingButton variant="secondary" busy={isBusy(`delete-snapshot:${snapshot.id}`)} disabled={restoringSnapshot !== null} onclick={() => deleteSnapshot(snapshot.id)}>{language.remove}</SettingButton>
                </div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/each}
        </details>
        <details data-storage-backup-list="conflict-backups" class="group">
            {@render listHeader(strings.conflictBackups, view.conflictBackups.length > 0 ? listSummary(view.conflictBackups.length, rollup.conflictBackupBytes) : '')}
            {#each view.conflictBackups as backup (backup.id)}
                <div data-storage-backup-row class={listRowClass}>
                    <span class="min-w-0 flex-1 break-words tabular-nums">{new Date(backup.createdAt).toLocaleString()}</span>
                    <span class="text-textcolor2 tabular-nums">{formatRisuNestStorageBytes(backup.byteLength)}</span>
                    <SettingButton variant="secondary" busy={isBusy(`delete-conflict-backup:${backup.id}`)} onclick={() => deleteConflictBackup(backup.id)}>{language.remove}</SettingButton>
                </div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/each}
        </details>
        <details data-storage-backup-list="sync-backups" class="group">
            {@render listHeader(strings.syncBackups, serverBackupCount > 0 ? listSummary(serverBackupCount, rollup.serverBackupBytes) : '')}
            {#if view.serverBackups && view.serverBackups.incompleteCount > 0}
                <p class="{listNoteClass} tabular-nums">{syncLabels.incomplete} ({formatCount(view.serverBackups.incompleteCount)}) · {formatRisuNestStorageBytes(view.serverBackups.incompleteBytes)}</p>
            {/if}
            {#each view.serverBackups?.items ?? [] as backup (backup.id)}
                {@const localBytes = backup.local.localRequiredBytes + backup.local.remoteDependentBytes}
                {@const remoteBytes = backup.remote.localRequiredBytes + backup.remote.remoteDependentBytes}
                <div data-storage-backup-row class={listRowClass}>
                    <span class="min-w-0 flex-1 break-words tabular-nums">{new Date(backup.createdAt).toLocaleString()}</span>
                    <span class="text-textcolor2 tabular-nums">{formatRisuNestStorageBytes(localBytes + remoteBytes)}</span>
                    <SettingButton variant="secondary" busy={isBusy(`restore-server-backup:${backup.id}:local`)} disabled={serverBackupBusy || backup.local.availability === 'unavailable' || Boolean(backup.blockedReason)} onclick={() => restoreServerBackup(backup.id, 'local')}>{syncText.restoreLocalBackup} ({formatRisuNestStorageBytes(localBytes)})</SettingButton>
                    <SettingButton variant="secondary" busy={isBusy(`restore-server-backup:${backup.id}:remote`)} disabled={serverBackupBusy || backup.remote.availability === 'unavailable' || Boolean(backup.blockedReason)} onclick={() => restoreServerBackup(backup.id, 'remote')}>{syncText.restoreRemoteBackup} ({formatRisuNestStorageBytes(remoteBytes)})</SettingButton>
                    <SettingButton variant="secondary" busy={isBusy(`export-server-backup:${backup.id}:local`)} disabled={serverBackupBusy || backup.local.availability === 'unavailable' || Boolean(backup.blockedReason)} onclick={() => exportServerBackup(backup.id, 'local')}>{syncText.exportLocalBackup}</SettingButton>
                    <SettingButton variant="secondary" busy={isBusy(`export-server-backup:${backup.id}:remote`)} disabled={serverBackupBusy || backup.remote.availability === 'unavailable' || Boolean(backup.blockedReason)} onclick={() => exportServerBackup(backup.id, 'remote')}>{syncText.exportRemoteBackup}</SettingButton>
                    <SettingButton variant="secondary" busy={isBusy(`delete-server-backup:${backup.id}`)} disabled={serverBackupBusy || !backup.deletable} onclick={() => deleteServerBackup(backup.id)}>{language.remove}</SettingButton>
                    {#if backup.blockedReason}<p class="basis-full text-xs text-textcolor2">{backup.blockedReason}</p>{/if}
                </div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/each}
            {#if view.serverBackups?.next}
                <div class="border-t border-darkborderc/55 py-2 pr-4 pl-10">
                    <SettingButton variant="secondary" busy={isBusy('more-server-backups')} onclick={loadMoreServerBackups}>{syncLabels.more}</SettingButton>
                </div>
            {/if}
        </details>
        <details data-storage-backup-list="temp-files" class="group">
            {@render listHeader(syncLabels.cache, rollup.cacheBytes > 0 ? formatRisuNestStorageBytes(rollup.cacheBytes) : '')}
            {#if view.tempUsage}
                <div data-storage-temp-row class={listRowClass}>
                    <span class="min-w-0 flex-1 text-textcolor2 tabular-nums">{syncLabels.protected} {formatRisuNestStorageBytes(view.tempUsage.protectedBytes)} · {syncLabels.reclaimable} {formatRisuNestStorageBytes(view.tempUsage.reclaimableBytes)}</span>
                    <SettingButton variant="secondary" busy={isBusy('cleanup-temp')} disabled={view.tempUsage.reclaimableBytes === 0 || Boolean(view.tempUsage.blockedReason)} onclick={cleanupTemp}>{syncLabels.clean}</SettingButton>
                    {#if view.tempUsage.blockedReason}<p class="basis-full text-xs text-textcolor2">{view.tempUsage.blockedReason}</p>{/if}
                </div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/if}
        </details>

        <div data-storage-action-row class="divide-y divide-darkborderc/55">
            <SettingRow data-storage-action="snapshot" label={strings.createSnapshotTitle} help={strings.createSnapshotHelp}>
                <SettingButton busy={isBusy('create-snapshot')} onclick={createSnapshot}>{strings.createSnapshot}</SettingButton>
            </SettingRow>
            <SettingRow data-storage-action="gc" label={strings.gcTitle} help={strings.gcHelp}>
                {#snippet below()}
                    <p class="mt-0.5 max-w-[62ch] text-[13px] leading-normal text-textcolor2">{strings.gcSeparation}</p>
                    <div role="status" aria-live="polite">
                        {#if view.gcPreview}
                            <p class="mt-1 text-sm tabular-nums">{strings.gcResult.replace('{0}', formatCount(view.gcPreview.candidateCount)).replace('{1}', formatRisuNestStorageBytes(view.gcPreview.candidateBytes))}</p>
                        {:else if view.gcResult}
                            <p class="mt-1 text-sm tabular-nums">{strings.gcDeletedResult.replace('{0}', formatCount(view.gcResult.deletedCount)).replace('{1}', formatRisuNestStorageBytes(view.gcResult.deletedBytes))}</p>
                        {/if}
                    </div>
                    {#if isBusy('preview-gc') || isBusy('execute-gc')}
                        <div data-storage-gc-progress class="mt-2">
                            <SettingProgress label={isBusy('execute-gc') ? strings.gcDeleting : strings.gcSearching} />
                        </div>
                    {/if}
                    {#if view.gcPreview?.candidates?.length}
                        {#snippet gcRow(candidate: NativeAssetGcCandidate)}
                            <div data-storage-gc-row class="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 px-3 py-1.5 text-sm">
                                <span class="font-mono text-xs break-all">{candidate.objectHash.slice(0, 12)}</span>
                                <span class="tabular-nums text-textcolor2">{formatRisuNestStorageBytes(candidate.bytes)}</span>
                                <span class="min-w-0 flex-1 text-textcolor2">{gcReason(candidate)}</span>
                            </div>
                        {/snippet}
                        {#if gcDeletable.length > 0}
                            <details data-storage-gc-list class="group mt-2" open>
                                <summary class="flex cursor-pointer list-none items-center gap-2 text-sm select-none [&::-webkit-details-marker]:hidden">
                                    <ChevronRight size={16} class="shrink-0 text-textcolor2 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" />
                                    <span>{strings.gcListTitle}</span>
                                    <span class="text-textcolor2 tabular-nums">{formatCount(gcDeletable.length)}</span>
                                </summary>
                                <div class="mt-1 divide-y divide-darkborderc/55 rounded-md border border-darkborderc/55">
                                    {#each gcDeletable as candidate (candidate.objectHash)}
                                        {@render gcRow(candidate)}
                                    {/each}
                                    {#if view.gcPreview.omitted}
                                        <p class="px-3 py-1.5 text-sm text-textcolor2">{strings.gcListMore.replace('{0}', formatCount(view.gcPreview.omitted))}</p>
                                    {/if}
                                </div>
                            </details>
                        {/if}
                        {#if gcKept.length > 0}
                            <details data-storage-gc-kept class="group mt-2">
                                <summary class="flex cursor-pointer list-none items-center gap-2 text-sm text-textcolor2 select-none [&::-webkit-details-marker]:hidden">
                                    <ChevronRight size={16} class="shrink-0 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" />
                                    <span>{strings.gcListKept.replace('{0}', formatCount(gcKept.length))}</span>
                                </summary>
                                <p class="mt-1 text-[13px] leading-normal text-textcolor2">{strings.gcListKeptHelp}</p>
                                <div class="mt-1 divide-y divide-darkborderc/55 rounded-md border border-darkborderc/55">
                                    {#each gcKept as candidate (candidate.objectHash)}
                                        {@render gcRow(candidate)}
                                    {/each}
                                </div>
                            </details>
                        {/if}
                    {/if}
                {/snippet}
                <SettingButton variant="secondary" busy={isBusy('preview-gc')} disabled={isBusy('execute-gc')} onclick={previewGc}>{strings.gcRun}</SettingButton>
                {#if view.gcPreview}
                    <SettingButton busy={isBusy('execute-gc')} onclick={executeGc}>{strings.gcRunConfirm}</SettingButton>
                {/if}
            </SettingRow>
        </div>
    {/if}
</SettingGroup>
