<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { ChevronRight } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertNormal } from 'src/ts/alert'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import SettingProgress from '../RisuNest/SettingProgress.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import NumberInput from 'src/lib/UI/GUI/NumberInput.svelte'
    import {
        getDeviceSettings,
        MAX_SNAPSHOT_KEEP_COUNT,
        subscribeDeviceSettings,
        updateDeviceSettings,
    } from 'src/ts/storage/deviceSettings'
    import {
        getServerSyncCacheUsage,
        cleanupServerSyncCache,
    } from 'src/ts/storage/sync/serverSyncProduction'
    import {
        createNativePersistentSnapshot,
        deleteNativePersistentSnapshot,
        executeNativePersistentAssetGc,
        getNativePersistentStorageStats,
        listNativePersistentSnapshots,
        previewNativePersistentAssetGc,
        restoreNativePersistentSnapshot,
    } from 'src/ts/storage/nativePersistentMaintenance'
    import { requestStoreCompaction } from 'src/ts/storage/storeCompaction'
    import { getSyncConflictBackupStore } from 'src/ts/storage/sync/syncConflictBackup'
    import { assetAliasCleanupTarget, flushPendingData } from 'src/ts/storage/persistentDataRuntime.svelte'
    import { deleteUnusedAssetAliases } from 'src/ts/storage/deletedOwnerAssetCleanup'
    import { getAssetResidencyStatus } from 'src/ts/storage/sync/serverAssetResidency'
    import { describeBlockedReason } from 'src/ts/storage/sync/blockedReasonText'
    import { openDataHealthScreen } from 'src/ts/storage/dataHealthNavigation'
    import type { NativeAssetGcCandidate } from 'src/ts/storage/nativePersistentMaintenance'
    import {
        createRisuNestStorageDashboard,
        formatRisuNestStorageBytes,
        storageDashboardRollup,
        storageOffDeviceParts,
        type RisuNestStorageCardId,
        type StorageOffDevicePart,
    } from 'src/ts/storage/risuNestStorageDashboard'

    const conflictStore = getSyncConflictBackupStore()
    const dashboard = createRisuNestStorageDashboard({
        getStats: getNativePersistentStorageStats,
        listSnapshots: listNativePersistentSnapshots,
        listConflictBackups: () => conflictStore.list(),
        getTemp: getServerSyncCacheUsage,
        getResidency: () => getAssetResidencyStatus(),
        cleanupTemp: cleanupServerSyncCache,
        // Pending edits are committed first so a file they started using counts as used.
        previewGc: () => flushPendingData('asset-gc-preview').then(previewNativePersistentAssetGc),
        executeGc: () => deleteUnusedAssetAliases(assetAliasCleanupTarget(), 'asset-gc-execute').then(executeNativePersistentAssetGc),
        deleteSnapshot: deleteNativePersistentSnapshot,
        deleteConflictBackup: (id) => conflictStore.remove(id),
        createSnapshot: createNativePersistentSnapshot,
    })
    let view = $state(dashboard.snapshot())
    // Restoring restarts the app, so the flag only ever clears on cancel or failure.
    let restoringSnapshot: string | null = $state(null)
    let requestingCompaction = $state(false)
    // The snapshot list is not part of the totals, so they show without it.
    let rollup = $derived(
        view.stats && view.loadedSources.includes('conflictBackups') && view.loadedSources.includes('tempUsage')
            ? storageDashboardRollup(
                  view.stats,
                  view.conflictBackups,
                  view.tempUsage,
              )
            : null,
    )
    const strings = language.risuNest.storage
    const syncText = language.risuNest.serverSync
    const syncLabels = syncText.management
    const offDeviceLabels: Record<StorageOffDevicePart, string> = {
        server: syncText.residency.remoteOnly,
        external: syncText.residency.externalOnly,
        unavailable: syncText.residency.unavailable,
    }
    const formatCount = (value: number): string => value.toLocaleString()
    const listSummary = (count: number, bytes: number): string => strings.listSummary
        .replace('{0}', formatCount(count))
        .replace('{1}', formatRisuNestStorageBytes(bytes))
    const cardBytes = (id: RisuNestStorageCardId): number => rollup?.cards.find((card) => card.id === id)?.bytes ?? 0
    let totalBytes = $derived(cardBytes('total'))
    // Built as one string so the separators keep their spaces.
    let offDeviceNote = $derived(
        view.residency
            ? storageOffDeviceParts(view.residency)
                  .map(({ part, value }) => `${offDeviceLabels[part]} ${part === 'unavailable'
                      ? syncText.count.replace('{0}', formatCount(value))
                      : formatRisuNestStorageBytes(value)}`)
                  .join(' · ')
            : '',
    )
    let mediaNote = $derived(view.residencyLoading || offDeviceNote !== '')
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
                      id: 'cache',
                      label: syncLabels.cache,
                      bytes: rollup.cacheBytes,
                      color: 'bg-neutral-500',
                  },
                  {
                      id: 'ledger',
                      label: syncLabels.ledger,
                      bytes: rollup.ledgerBytes,
                      color: 'bg-primary-500',
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
        if (reason === "data-health-repair") return reasons.dataHealthRepair
        return ""
    }

    // Another operation holding the library refused the action; it works once that operation ends.
    const refusedAsBusy = (error: unknown): boolean =>
        typeof error === 'object' && error !== null && 'code' in error
        && (error.code === 'library-operation-busy' || error.code === 'commit-busy')

    function showActionError(error: unknown, fallback = strings.actionFailed): void {
        alertError(refusedAsBusy(error) ? language.risuNest.importDialog.reasonBusy : fallback)
    }

    // Creating a snapshot or reading an image fails on the same damage the data check names.
    async function showStorageFailure(error: unknown): Promise<void> {
        if (refusedAsBusy(error)) {
            showActionError(error)
            return
        }
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
        blocked: strings.gcBlocked,
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
                onEmpty: () => { listed = false },
            })
        } catch (error) {
            const syncUnavailable = typeof error === 'object' && error !== null && 'code' in error && error.code === 'sync-unavailable'
            showActionError(error, syncUnavailable ? language.risuNest.backup.syncUnavailable : strings.actionFailed)
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

    async function cleanupTemp(): Promise<void> {
        if (!await alertConfirm(syncLabels.cleanConfirm)) return
        try { await dashboard.cleanupTemp() } catch (error) { showActionError(error) }
    }

    async function compactDatabase(): Promise<void> {
        if (requestingCompaction || !await alertConfirm(strings.compactConfirm)) return
        requestingCompaction = true
        try { await requestStoreCompaction() } catch (error) { showActionError(error) } finally { requestingCompaction = false }
    }

    async function createSnapshot(): Promise<void> {
        try { await dashboard.createSnapshot() } catch (error) { await showStorageFailure(error) }
    }

    const initialSettings = getDeviceSettings()
    let automaticSnapshots = $state(initialSettings.automaticSnapshotsEnabled)
    let storedKeepCount = initialSettings.snapshotKeepCount
    let keepCount = $state(initialSettings.snapshotKeepCount)
    const unsubscribeSettings = subscribeDeviceSettings((settings) => {
        automaticSnapshots = settings.automaticSnapshotsEnabled
        storedKeepCount = settings.snapshotKeepCount
        keepCount = settings.snapshotKeepCount
    })

    function setAutomaticSnapshots(next: boolean): void {
        if (next === automaticSnapshots) return
        automaticSnapshots = next
        updateDeviceSettings({ automaticSnapshotsEnabled: next })
    }

    function commitKeepCount(): void {
        const value = Number(keepCount)
        if (keepCount === null || !Number.isFinite(value)) {
            keepCount = storedKeepCount
            return
        }
        const next = Math.min(MAX_SNAPSHOT_KEEP_COUNT, Math.max(1, Math.round(value)))
        keepCount = next
        if (next === storedKeepCount) return
        storedKeepCount = next
        updateDeviceSettings({ snapshotKeepCount: next })
    }

    const unsubscribe = dashboard.subscribe((next) => { view = next })
    onMount(() => { void dashboard.load() })
    onDestroy(() => {
        unsubscribe()
        unsubscribeSettings()
    })
</script>

{#snippet listHeader(label: string, summary: string)}
    <summary class={listHeaderClass}>
        <ChevronRight size={16} class="shrink-0 text-textcolor2 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" />
        <span>{label}</span>
        {#if summary}<span class="ml-auto text-sm text-textcolor2 tabular-nums">{summary}</span>{/if}
    </summary>
{/snippet}

<SettingGroup id="risunest-storage" title={strings.title}>
    {#snippet actions()}
        <SettingButton variant="secondary" busy={view.loading} onclick={() => dashboard.load()}>{strings.refresh}</SettingButton>
    {/snippet}
    {#if view.loadFailed}
        <p class="px-4 py-3 text-sm text-textcolor2" role="alert" aria-live="assertive">{rollup ? strings.staleTotals : strings.loadFailed}</p>
    {/if}
    {#if view.loading && !rollup}
        <!-- The placeholder keeps the loaded shape: the total, the bar, and one legend line. -->
        <div data-storage-summary-placeholder class="p-4" role="status" aria-live="polite" aria-label={language.loading}>
            <div class="h-9 w-40 animate-pulse rounded-md bg-darkbutton" aria-hidden="true"></div>
            <div class="mt-3 h-3 w-full animate-pulse rounded-full bg-darkbutton" aria-hidden="true"></div>
            <div class="mt-3 grid grid-cols-[repeat(auto-fit,minmax(min(100%,14rem),1fr))] gap-x-5 gap-y-1.5" aria-hidden="true">
                {#each Array(3) as _}
                    <div class="h-4 animate-pulse rounded-xs bg-darkbutton"></div>
                {/each}
            </div>
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
            <!-- Each column fits a label and its value on one line; a narrow panel shows one column. -->
            <ul data-storage-legend class="mt-3 grid grid-cols-[repeat(auto-fit,minmax(min(100%,14rem),1fr))] gap-x-5 gap-y-1.5 text-sm">
                {#each segments as segment (segment.id)}
                    <!-- The media row takes the whole line while it carries the note, so the note stays on one line. -->
                    <li class="flex flex-col {segment.id === 'media' && mediaNote ? 'col-span-full' : ''}">
                        <div class="flex items-center gap-2">
                            <span class="h-2.5 w-2.5 shrink-0 rounded-xs {segment.color}" aria-hidden="true"></span>
                            <span class="min-w-0 flex-1 text-textcolor2 break-keep">{segment.label}</span>
                            <span class="shrink-0 whitespace-nowrap tabular-nums">{formatRisuNestStorageBytes(segment.bytes)}</span>
                        </div>
                        <!-- The placeholder holds the note's line so the legend does not move when the note arrives. -->
                        {#if segment.id === 'media' && view.residencyLoading}
                            <div data-storage-off-device-placeholder class="flex h-4 items-center pl-[18px]" aria-hidden="true">
                                <span class="h-2.5 w-32 max-w-full animate-pulse rounded-xs bg-darkbutton"></span>
                            </div>
                        {:else if segment.id === 'media' && offDeviceNote}
                            <div data-storage-off-device class="pl-[18px] text-xs text-textcolor2 tabular-nums">{offDeviceNote}</div>
                        {/if}
                    </li>
                {/each}
            </ul>
            <p class="mt-2 text-xs text-textcolor2">
                {strings.subMetrics
                    .replace('{0}', formatRisuNestStorageBytes(cardBytes('inlays')))
                    .replace('{1}', formatRisuNestStorageBytes(cardBytes('plugins')))}
            </p>
        </div>

    {/if}
        {#if view.loadedSources.includes('snapshots')}
        <details data-storage-backup-list="snapshots" class="group">
            {@render listHeader(strings.snapshots, view.snapshots.length > 0 && view.stats ? listSummary(view.snapshots.length, view.stats?.snapshotBytes ?? 0) : '')}
            <p class={listNoteClass}>{strings.snapshotSizeNote}</p>
            {#if view.failedSources.includes('snapshots')}<p role="alert" class={listNoteClass}>{strings.loadFailed}</p>{/if}
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
        {/if}
        {#if view.loadedSources.includes('conflictBackups')}
        <details data-storage-backup-list="conflict-backups" class="group">
            {@render listHeader(strings.conflictBackups, view.conflictBackups.length > 0 ? listSummary(view.conflictBackups.length, view.conflictBackups.reduce((sum, backup) => sum + backup.byteLength, 0)) : '')}
            {#if view.failedSources.includes('conflictBackups')}<p role="alert" class={listNoteClass}>{strings.loadFailed}</p>{/if}
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
        {/if}
        {#if view.loadedSources.includes('tempUsage')}
        <details data-storage-backup-list="temp-files" class="group">
            {@render listHeader(syncLabels.cache, (view.tempUsage?.cacheBytes ?? 0) > 0 ? formatRisuNestStorageBytes((view.tempUsage?.cacheBytes ?? 0)) : '')}
            {#if view.tempUsage}
                <div data-storage-temp-row class="grid items-center gap-x-6 gap-y-2 border-t border-darkborderc/55 py-2 pr-4 pl-10 text-sm @xl:grid-cols-[minmax(50%,1fr)_auto]">
                    <div class="min-w-0">
                        <span class="text-textcolor2 tabular-nums">{syncLabels.protected} {formatRisuNestStorageBytes(view.tempUsage.protectedBytes)} · {syncLabels.reclaimable} {formatRisuNestStorageBytes(view.tempUsage.reclaimableBytes)}</span>
                        {#if view.tempUsage.blockedReason}<p class="mt-0.5 text-xs text-textcolor2">{describeBlockedReason(view.tempUsage.blockedReason)}</p>{/if}
                    </div>
                    <div class="flex flex-wrap items-center gap-2 @xl:justify-self-end">
                        <SettingButton variant="secondary" busy={isBusy('cleanup-temp')} disabled={view.tempUsage.reclaimableBytes === 0 || Boolean(view.tempUsage.blockedReason)} onclick={cleanupTemp}>{syncLabels.clean}</SettingButton>
                    </div>
                </div>
            {:else}
                <p class={listEmptyClass}>{strings.emptyList}</p>
            {/if}
        </details>
        {/if}

    <SettingRow inline data-storage-setting="automatic-snapshots" label={strings.automaticSnapshots} help={strings.automaticSnapshotsHelp}>
        <SettingToggle checked={automaticSnapshots} onchange={setAutomaticSnapshots} label={strings.automaticSnapshots} />
    </SettingRow>
    <SettingRow data-storage-setting="snapshot-keep-count" label={strings.snapshotKeepCount} help={strings.snapshotKeepCountHelp.replace('{0}', formatCount(MAX_SNAPSHOT_KEEP_COUNT))}>
        <NumberInput
            size="sm"
            min={1}
            max={MAX_SNAPSHOT_KEEP_COUNT}
            ariaLabel={strings.snapshotKeepCount}
            className="w-28 text-right"
            bind:value={keepCount}
            onChange={commitKeepCount}
        />
    </SettingRow>
    {#if view.stats}
        <SettingRow data-storage-action="snapshot" label={strings.createSnapshotTitle} help={strings.createSnapshotHelp}>
            <SettingButton busy={isBusy('create-snapshot')} onclick={createSnapshot}>{strings.createSnapshot}</SettingButton>
        </SettingRow>
        <SettingRow data-storage-action="gc" label={strings.gcTitle} help={strings.gcHelp}>
            {#snippet below()}
                <!-- The controls stay above the lists they produce instead of beside them. -->
                <div class="mt-2 flex flex-wrap items-center gap-2">
                    <SettingButton variant="secondary" busy={isBusy('preview-gc')} disabled={isBusy('execute-gc')} onclick={previewGc}>{strings.gcRun}</SettingButton>
                    {#if view.gcPreview && view.gcPreview.blockers.length === 0 && view.gcPreview.candidateCount > 0}
                        <SettingButton busy={isBusy('execute-gc')} onclick={executeGc}>{strings.gcRunConfirm}</SettingButton>
                    {/if}
                </div>
                <div role="status" aria-live="polite">
                    {#if view.gcPreview?.blockers.length}
                        <p class="mt-1 text-sm">{strings.gcBlocked} {view.gcPreview.blockers.includes('plugin-storage-opaque') ? strings.gcPluginBlocked : ''}</p>
                    {:else if view.gcPreview}
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
        </SettingRow>
        <SettingRow data-storage-action="compact" label={strings.compactTitle} help={strings.compactHelp}>
            <span class="text-sm text-textcolor2 tabular-nums">{strings.compactReclaimable.replace('{0}', formatRisuNestStorageBytes(view.stats.databaseReclaimableBytes))}</span>
            <SettingButton variant="secondary" busy={requestingCompaction} disabled={view.stats.databaseReclaimableBytes === 0} onclick={compactDatabase}>{strings.compact}</SettingButton>
        </SettingRow>
    {/if}
</SettingGroup>
