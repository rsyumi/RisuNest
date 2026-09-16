<script lang="ts">
    import { onDestroy } from 'svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertNormal } from 'src/ts/alert'
    import { isTauri, isTauriAndroid, isTauriDesktop } from 'src/ts/platform'
    import { LoadLocalBackup } from 'src/ts/drive/backuplocal'
    import { openSyncConflictBackups } from 'src/ts/storage/sync/syncConflictRestore'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import { getNativeOfficialAccountFlow } from 'src/ts/storage/sync/nativeOfficialAccountFlow'
    import { DBState } from 'src/ts/stores.svelte'
    import {
        nativeFileOperation,
        importRisuSaveFromSystemPicker,
        exportRisuSaveFromSystemPicker,
    } from 'src/ts/storage/risuSaveFileRouteProduction.svelte'
    import {
        collectExportExcludedReport,
        formatExportExcludedReport,
        isEmptyExportExcludedReport,
    } from 'src/ts/storage/exportExcludedReport'
    import { restoreBackupFromSystemPicker } from 'src/ts/storage/portableBackupFileRouteProduction.svelte'
    import {
        alertPartialDestinationWarning,
        hasPartialDestinationWarning,
    } from 'src/ts/storage/risuSaveFileRoute'
    import {
        NativeFileJobActivationCommittedError,
        NativeFileJobError,
    } from 'src/ts/storage/nativeFileJobs'
    import {
        cancelActiveNativeFileOperation,
    } from 'src/ts/storage/nativeFileJobManager'
    import { nativeFileJobProgressText } from 'src/ts/gui/nativeFileJobProgress'
    import ExternalStorageSettings from '../ExternalStorage/ExternalStorageSettings.svelte'

    let nativeAccountBusy = $state(false)
    let nativePublishController = $state<AbortController | null>(null)
    let risuSaveOperation = $derived($nativeFileOperation?.kind ?? null)
    let risuSaveStatus = $derived($nativeFileOperation?.status)
    // Imports open the shared progress dialog; only inline operations (exports) use this row.
    let inlineOperation = $derived(
        $nativeFileOperation?.presentation === 'inline',
    )

    function showRisuSaveError(error: unknown): void {
        const partialDestinationMayRemain = hasPartialDestinationWarning(error)
        if (error instanceof DOMException && error.name === 'AbortError') {
            alertPartialDestinationWarning(
                error,
                language.screenshotPartialDestinationMayRemain,
                alertError,
            )
            return
        }
        if (error instanceof NativeFileJobActivationCommittedError) {
            alertError(language.risuSaveImportCommittedRefreshFailed)
            return
        }
        if (
            error instanceof NativeFileJobError &&
            error.code === 'revision-conflict'
        ) {
            alertError(language.risuSaveRevisionConflict)
            return
        }
        alertError(
            partialDestinationMayRemain
                ? `${language.risuNest.backup.actionFailed} ${language.screenshotPartialDestinationMayRemain}`
                : language.risuNest.backup.actionFailed,
        )
    }

    async function runRisuSaveOperation(
        kind: 'import' | 'export',
    ): Promise<void> {
        if (risuSaveOperation) return
        if (
            kind === 'import' &&
            !isTauri &&
            (!(await alertConfirm(language.risuSaveImportConfirm)) ||
                !(await alertConfirm(language.backupLoadConfirm2)))
        )
            return
        if (kind === 'import') {
            // The shared progress dialog reports progress, cancellation, and the outcome.
            try {
                if (isTauri) await restoreBackupFromSystemPicker()
                else await importRisuSaveFromSystemPicker()
            } catch (error) {
                showRisuSaveError(error)
            }
            return
        }
        try {
            const excluded = await collectExportExcludedReport(DBState.db.characters)
            const result = await exportRisuSaveFromSystemPicker()
            if (!result) return
            alertNormal(
                result.warningCodes.includes('cleanup-failed')
                    ? language.risuSaveCleanupWarning
                    : isEmptyExportExcludedReport(excluded)
                        ? language.risuSaveExportComplete
                        : formatExportExcludedReport(excluded),
            )
        } catch (error) {
            showRisuSaveError(error)
        }
    }

    async function runNativeAccountOperation<T>(
        operation: () => Promise<T>,
    ): Promise<T | undefined> {
        if (nativeAccountBusy) return undefined
        nativeAccountBusy = true
        try {
            return await operation()
        } finally {
            nativeAccountBusy = false
        }
    }

    async function loadPocketRisuBackup(): Promise<void> {
        if (isTauri) return runRisuSaveOperation('import')
        if (
            (await alertConfirm(language.pocketRisuImportConfirm)) &&
            (await alertConfirm(language.backupLoadConfirm2))
        )
            LoadLocalBackup()
    }

    function restoreOfficialBackup(): Promise<void | undefined> {
        return runNativeAccountOperation(async () => {
            if (
                !(await alertConfirm(
                    language.risuNest.backup.officialRestoreConfirm,
                ))
            )
                return
            if (
                !(await alertConfirm(
                    language.risuNest.backup.officialRestoreInlayWarning,
                ))
            )
                return
            try {
                const result = await getNativeOfficialAccountFlow().restore()
                if (result.kind === 'missing')
                    alertNormal(language.risuNest.backup.officialMissing)
            } catch {
                alertError(language.risuNest.backup.actionFailed)
            }
        })
    }

    function publishOfficialBackup(): Promise<void | undefined> {
        return runNativeAccountOperation(async () => {
            if (
                !(await alertConfirm(
                    language.risuNest.backup.officialPublishConfirm,
                ))
            )
                return
            const controller = new AbortController()
            nativePublishController = controller
            try {
                await getNativeOfficialAccountFlow().publish(controller.signal)
                alertNormal(language.risuNest.backup.officialPublished)
            } catch (error) {
                if (!(
                    error instanceof DOMException && error.name === 'AbortError'
                ))
                    alertError(language.risuNest.backup.actionFailed)
            } finally {
                if (nativePublishController === controller)
                    nativePublishController = null
            }
        })
    }

    onDestroy(() => nativePublishController?.abort())
</script>

<SettingGroup id="risunest-backup" title={language.risuNest.backup.title}>
    <SettingRow
        data-backup-group="files"
        label={language.risuNest.backup.groupFiles}
        help={language.risuNest.backup.filesHelp}
    >
        {#snippet below()}
            {#if risuSaveOperation && inlineOperation}
                <div
                    class="mt-1 flex flex-wrap items-center gap-2 text-sm text-textcolor2"
                    role="status"
                    aria-live="polite"
                >
                    <span>{nativeFileJobProgressText(risuSaveStatus)}</span>
                    <SettingButton
                        variant="secondary"
                        onclick={cancelActiveNativeFileOperation}
                        >{language.cancelRisuSaveOperation}</SettingButton
                    >
                </div>
            {/if}
        {/snippet}
        {#if !isTauri || isTauriDesktop || isTauriAndroid}
            <SettingButton
                busy={risuSaveOperation === 'import'}
                disabled={risuSaveOperation !== null}
                onclick={() => runRisuSaveOperation('import')}
                >{language.risuNest.backup.importFile}</SettingButton
            >
        {/if}
        {#if !isTauri || isTauriDesktop || isTauriAndroid}
            <SettingButton
                busy={risuSaveOperation === 'export'}
                disabled={risuSaveOperation !== null}
                onclick={() => runRisuSaveOperation('export')}
                >{language.risuNest.backup.exportFile}</SettingButton
            >
        {/if}
    </SettingRow>
    <SettingRow
        data-backup-group="restore"
        label={language.risuNest.backup.groupRestore}
        help={language.risuNest.backup.restoreHelp}
    >
        <SettingButton
            disabled={risuSaveOperation !== null}
            onclick={loadPocketRisuBackup}
            >{language.loadPocketRisuBackup}</SettingButton
        >
        <SettingButton variant="secondary" onclick={() => openSyncConflictBackups()}
            >{language.syncConflictBackups}</SettingButton
        >
    </SettingRow>
    {#if isTauri && DBState.db.account}
        <SettingRow
            data-backup-group="account"
            label={language.risuNest.backup.groupAccount}
            help={language.risuNest.backup.accountHelp}
        >
            <SettingButton
                busy={nativePublishController !== null}
                disabled={nativeAccountBusy}
                onclick={publishOfficialBackup}
                >{language.risuNest.backup.officialPublish}</SettingButton
            >
            <SettingButton
                busy={nativeAccountBusy && nativePublishController === null}
                disabled={nativeAccountBusy}
                onclick={restoreOfficialBackup}
                >{language.risuNest.backup.officialRestore}</SettingButton
            >
            {#if nativePublishController}
                <SettingButton
                    variant="secondary"
                    onclick={() => nativePublishController?.abort()}
                    >{language.risuNest.backup.officialCancel}</SettingButton
                >
            {/if}
        </SettingRow>
    {/if}
</SettingGroup>
<ExternalStorageSettings />
