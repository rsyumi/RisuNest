<script lang="ts">
    import { onDestroy } from 'svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertNormal, alertSelect } from 'src/ts/alert'
    import { isTauri, isTauriAndroid, isTauriDesktop } from 'src/ts/platform'
    import { LoadLocalBackup } from 'src/ts/drive/backuplocal'
    import { openSyncConflictBackups } from 'src/ts/storage/sync/syncConflictRestore'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import { restoreNativePersistentSnapshot, restartNativeApp } from 'src/ts/storage/nativePersistentMaintenance'
    import { getNativeOfficialAccountFlow } from 'src/ts/storage/sync/nativeOfficialAccountFlow'
    import { DBState } from 'src/ts/stores.svelte'
    import { nativeFileOperation, importRisuSaveFromSystemPicker, exportRisuSaveFromSystemPicker } from 'src/ts/storage/risuSaveFileRouteProduction.svelte'
    import { alertPartialDestinationWarning, hasPartialDestinationWarning } from 'src/ts/storage/risuSaveFileRoute'
    import { NativeFileJobActivationCommittedError, NativeFileJobError, type NativeFileJobStatus } from 'src/ts/storage/nativeFileJobs'
    import { cancelActiveNativeFileOperation } from 'src/ts/storage/nativeFileJobManager'

    let nativeAccountBusy = $state(false)
    let nativePublishController = $state<AbortController | null>(null)
    let risuSaveOperation = $derived($nativeFileOperation?.kind ?? null)
    let risuSaveStatus = $derived($nativeFileOperation?.status)

    function risuSaveProgressText(status: NativeFileJobStatus | undefined): string {
        if (!status) return ''
        const label = status.phase === 'reading-source' || status.phase === 'writing-export' || status.phase === 'uploading-database'
            ? language.risuNest.backup.progressTransferring
            : status.phase === 'activating-database' || status.phase === 'publishing-destination' || status.phase === 'finalizing-publication' || status.phase === 'finalizing-export' || status.phase === 'complete'
                ? language.risuNest.backup.progressFinalizing
                : language.risuNest.backup.progressPreparing
        const total = status.progress.totalBytes
        if (total && total > 0) return `${label}: ${Math.min(100, Math.round(status.progress.completedBytes * 100 / total))}%`
        const bytes = status.progress.completedBytes
        return bytes > 0 ? `${label}: ${(bytes / (1024 * 1024)).toFixed(1)} MiB` : label
    }

    function showRisuSaveError(error: unknown): void {
        const partialDestinationMayRemain = hasPartialDestinationWarning(error)
        if (error instanceof DOMException && error.name === 'AbortError') {
            alertPartialDestinationWarning(error, language.screenshotPartialDestinationMayRemain, alertError)
            return
        }
        if (error instanceof NativeFileJobActivationCommittedError) {
            alertError(language.risuSaveImportCommittedRefreshFailed)
            return
        }
        if (error instanceof NativeFileJobError && error.code === 'revision-conflict') {
            alertError(language.risuSaveRevisionConflict)
            return
        }
        alertError(partialDestinationMayRemain
            ? `${language.risuNest.backup.actionFailed} ${language.screenshotPartialDestinationMayRemain}`
            : language.risuNest.backup.actionFailed)
    }

    async function runRisuSaveOperation(kind: 'import' | 'export'): Promise<void> {
        if (risuSaveOperation) return
        if (kind === 'import' && (!await alertConfirm(language.risuSaveImportConfirm) || !await alertConfirm(language.backupLoadConfirm2))) return
        try {
            const result = kind === 'import' ? await importRisuSaveFromSystemPicker() : await exportRisuSaveFromSystemPicker()
            if (!result) return
            alertNormal(result.warningCodes.includes('cleanup-failed') ? language.risuSaveCleanupWarning : kind === 'import' ? language.risuSaveImportComplete : language.risuSaveExportComplete)
        } catch (error) {
            showRisuSaveError(error)
        }
    }

    async function runNativeAccountOperation<T>(operation: () => Promise<T>): Promise<T | undefined> {
        if (nativeAccountBusy) return undefined
        nativeAccountBusy = true
        try {
            return await operation()
        } finally {
            nativeAccountBusy = false
        }
    }

    onDestroy(() => nativePublishController?.abort())
</script>

<h2 class="mb-2 text-2xl font-bold mt-6">{language.risuNest.backup.title}</h2>
{#if !isTauri || isTauriDesktop}
    <Button disabled={risuSaveOperation !== null} onclick={() => runRisuSaveOperation('import')} className="mt-2">{language.importRisuSave}</Button>
{/if}
{#if !isTauri || isTauriDesktop || isTauriAndroid}
    <Button disabled={risuSaveOperation !== null} onclick={() => runRisuSaveOperation('export')} className="mt-2">{language.exportRisuSave}</Button>
{/if}
{#if risuSaveOperation}
    <div class="mt-2 flex items-center gap-2 text-sm text-textcolor2" role="status" aria-live="polite">
        <span>{risuSaveProgressText(risuSaveStatus)}</span>
        <Button styled="outlined" size="sm" onclick={cancelActiveNativeFileOperation}>{language.cancelRisuSaveOperation}</Button>
    </div>
{/if}
{#if isTauri}
    <Button onclick={async () => {
        try {
            await restoreNativePersistentSnapshot({
                choose: async (snapshots) => {
                    const labels = snapshots.map((snapshot) => `${new Date(snapshot.modifiedAt).toLocaleString()} (${snapshot.bytes / (1024 * 1024) >= 1 ? `${(snapshot.bytes / (1024 * 1024)).toFixed(1)} MiB` : `${Math.max(1, Math.round(snapshot.bytes / 1024))} KiB`})`)
                    const selected = Number(await alertSelect([...labels, language.cancel], language.chooseLocalSnapshot))
                    return snapshots[selected]?.path ?? null
                },
                confirm: () => alertConfirm(language.restoreLocalSnapshotConfirm),
                restart: restartNativeApp,
                onEmpty: () => alertNormal(language.noLocalSnapshots),
            })
        } catch { alertError(language.risuNest.backup.actionFailed) }
    }} className="mt-2">{language.restoreLocalSnapshot}</Button>
{/if}
<Button onclick={async () => { if ((await alertConfirm(language.pocketRisuImportConfirm)) && (await alertConfirm(language.backupLoadConfirm2))) LoadLocalBackup() }} className="mt-2">{language.loadPocketRisuBackup}</Button>
<Button onclick={() => openSyncConflictBackups()} className="mt-2">{language.syncConflictBackups}</Button>
{#if isTauri && DBState.db.account}
    <Button disabled={nativeAccountBusy} onclick={() => runNativeAccountOperation(async () => {
        if (!await alertConfirm(language.risuNest.backup.officialRestoreConfirm)) return
        if (!await alertConfirm(language.risuNest.backup.officialRestoreInlayWarning)) return
        try {
            const result = await getNativeOfficialAccountFlow().restore()
            if (result.kind === 'missing') alertNormal(language.risuNest.backup.officialMissing)
        } catch { alertError(language.risuNest.backup.actionFailed) }
    })} className="mt-2">{language.risuNest.backup.officialRestore}</Button>
    <Button disabled={nativeAccountBusy} onclick={() => runNativeAccountOperation(async () => {
        if (!await alertConfirm(language.risuNest.backup.officialPublishConfirm)) return
        const controller = new AbortController()
        nativePublishController = controller
        try { await getNativeOfficialAccountFlow().publish(controller.signal); alertNormal(language.risuNest.backup.officialPublished) }
        catch (error) { if (!(error instanceof DOMException && error.name === 'AbortError')) alertError(language.risuNest.backup.actionFailed) }
        finally { if (nativePublishController === controller) nativePublishController = null }
    })} className="mt-2">{language.risuNest.backup.officialPublish}</Button>
    {#if nativePublishController}
        <Button onclick={() => nativePublishController?.abort()} className="mt-2">{language.risuNest.backup.officialCancel}</Button>
    {/if}
{/if}
