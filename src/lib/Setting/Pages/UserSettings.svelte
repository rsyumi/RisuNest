<script lang="ts">
    import { language } from "src/lang";
    import { hubURL } from "src/ts/characterCards";
    import { loadRisuAccountBackup, loadRisuAccountData, saveRisuAccountData } from "src/ts/drive/accounter";
    
    import { DBState } from 'src/ts/stores.svelte';
    import Check from "src/lib/UI/GUI/CheckInput.svelte";
    import { alertConfirm, alertError, alertNormal, alertSelect } from "src/ts/alert";
    import { forageStorage } from "src/ts/globalApi.svelte";
    import { isTauri, isNodeServer, isTauriDesktop } from "src/ts/platform"
    import { unMigrationAccount } from "src/ts/storage/accountStorage";
    import { checkDriver } from "src/ts/drive/drive";
    import { LoadLocalBackup, SaveLocalBackup, SavePartialLocalBackup } from "src/ts/drive/backuplocal";
    import { openSyncConflictBackups } from "src/ts/storage/sync/syncConflictRestore";
    import Button from "src/lib/UI/GUI/Button.svelte";
    import { exportAsDataset } from "src/ts/storage/exportAsDataset";
    import { loginToSionyw, testSionywLogin } from "src/ts/sionyw";
    import { cleanColdStorage } from "src/ts/process/coldstorage.svelte";
    import {
        restartNativeApp,
        restoreNativePersistentSnapshot,
    } from "src/ts/storage/nativePersistentMaintenance";
    import { getNativeOfficialAccountFlow } from "src/ts/storage/sync/nativeOfficialAccountFlow";
    import {
        createHubPopupController,
        isExpectedHubMessage,
        resolveExpectedOfficialAccountMessageUrl,
    } from "src/ts/storage/officialAccountMessage";
    import {
        NativeFileJobActivationCommittedError,
        NativeFileJobError,
        type NativeFileJobStatus,
    } from "src/ts/storage/nativeFileJobs";
    import {
        exportRisuSaveFromSystemPicker,
        importRisuSaveFromSystemPicker,
        nativeFileOperation,
    } from "src/ts/storage/risuSaveFileRouteProduction.svelte";
    import { cancelActiveNativeFileOperation } from "src/ts/storage/nativeFileJobManager";
    import { onDestroy } from "svelte";
    let openIframe = $state(false)
    let openIframeURL = $state('')
    const drivePopup = createHubPopupController()
    let accountIframe = $state<HTMLIFrameElement>()
    let nativeAccountBusy = $state(false)
    let risuSaveOperation = $derived($nativeFileOperation?.kind ?? null)
    let risuSaveStatus = $derived($nativeFileOperation?.status)

    async function runNativeAccountOperation<T>(operation: () => Promise<T>): Promise<T | undefined> {
        if (nativeAccountBusy) return undefined
        nativeAccountBusy = true
        try {
            return await operation()
        } finally {
            nativeAccountBusy = false
        }
    }

    function risuSaveProgressText(status: NativeFileJobStatus | undefined): string {
        if(!status) return ''
        const total = status.progress.totalBytes
        if(total && total > 0) {
            const percent = Math.min(100, Math.round(status.progress.completedBytes * 100 / total))
            return `${status.phase}: ${percent}%`
        }
        const bytes = status.progress.completedBytes
        return bytes > 0 ? `${status.phase}: ${(bytes / (1024 * 1024)).toFixed(1)} MiB` : status.phase
    }

    function showRisuSaveError(error: unknown): void {
        if(error instanceof DOMException && error.name === 'AbortError') return
        if(error instanceof NativeFileJobActivationCommittedError) {
            alertError(language.risuSaveImportCommittedRefreshFailed)
            return
        }
        if(error instanceof NativeFileJobError && error.code === 'revision-conflict') {
            alertError(language.risuSaveRevisionConflict)
            return
        }
        alertError(error instanceof Error ? error.message : String(error))
    }

    async function runRisuSaveOperation(kind: 'import' | 'export'): Promise<void> {
        if(risuSaveOperation) return
        if(kind === 'import') {
            if(!await alertConfirm(language.risuSaveImportConfirm)) return
            if(!await alertConfirm(language.backupLoadConfirm2)) return
        }
        try {
            const result = kind === 'import'
                ? await importRisuSaveFromSystemPicker()
                : await exportRisuSaveFromSystemPicker()
            if(!result) return
            alertNormal(
                result.warningCodes.includes('cleanup-failed')
                    ? language.risuSaveCleanupWarning
                    : kind === 'import'
                        ? language.risuSaveImportComplete
                        : language.risuSaveExportComplete,
            )
        } catch(error) {
            showRisuSaveError(error)
        }
    }

    onDestroy(() => {
        drivePopup.close()
    })
</script>

<svelte:window onmessage={async (e) => {
    const message = e.data?.msg
    const expectedUrl = resolveExpectedOfficialAccountMessageUrl(
        message?.type,
        hubURL,
        openIframeURL,
    )
    const expectedSource = message?.type === 'drive'
        ? drivePopup.source
        : accountIframe?.contentWindow
    if(!isExpectedHubMessage(e, expectedUrl, expectedSource)) return
    if(message?.type === 'drive'){
        if(!isTauri) await loadRisuAccountData()
        DBState.db.account.data.refresh_token = message.data.refresh_token
        DBState.db.account.data.access_token = message.data.access_token
        DBState.db.account.data.expires_in = (message.data.expires_in * 700) + Date.now()
        if(!isTauri) await saveRisuAccountData()
        drivePopup.close()
    }
    else if(message?.data.vaild){
        openIframe = false
        const credential = {
            id: message.id,
            token: message.token,
            data: message.data
        }
        DBState.db.account = isTauri
            ? await getNativeOfficialAccountFlow().login(credential)
            : credential
    }
}}></svelte:window>


<h2 class="mb-2 text-2xl font-bold mt-2">{language.account} & {language.files}</h2>

<Button
    onclick={async () => {
        if(await alertConfirm(language.backupConfirm)){
            SaveLocalBackup()
        }
    }} className="mt-2">
    {language.saveBackupLocal}
</Button>

<Button
    onclick={async () => {
        if(await alertConfirm(language.backupConfirm)){
            SavePartialLocalBackup()
        }
    }} className="mt-2">
    {language.savePartialLocalBackup}
</Button>

<Button
    onclick={async () => {
        if((await alertConfirm(language.backupLoadConfirm)) && (await alertConfirm(language.backupLoadConfirm2))){
            LoadLocalBackup()
        }
    }} className="mt-2">
    {language.loadBackupLocal}
</Button>

{#if !isTauri || isTauriDesktop}
    <Button
        disabled={risuSaveOperation !== null}
        onclick={() => runRisuSaveOperation('import')}
        className="mt-2">
        {language.importRisuSave}
    </Button>

    <Button
        disabled={risuSaveOperation !== null}
        onclick={() => runRisuSaveOperation('export')}
        className="mt-2">
        {language.exportRisuSave}
    </Button>

    {#if risuSaveOperation}
        <div class="mt-2 flex items-center gap-2 text-sm text-textcolor2">
            <span>{risuSaveProgressText(risuSaveStatus)}</span>
            <Button
                styled="outlined"
                size="sm"
                onclick={cancelActiveNativeFileOperation}>
                {language.cancelRisuSaveOperation}
            </Button>
        </div>
    {/if}
{/if}

{#if isTauri}
    <Button
        onclick={async () => {
            try {
                await restoreNativePersistentSnapshot({
                    choose: async (snapshots) => {
                        const labels = snapshots.map((snapshot) => {
                            const date = new Date(snapshot.modifiedAt).toLocaleString()
                            const mib = snapshot.bytes / (1024 * 1024)
                            const size = mib >= 1
                                ? `${mib.toFixed(1)} MiB`
                                : `${Math.max(1, Math.round(snapshot.bytes / 1024))} KiB`
                            return `${date} (${size})`
                        })
                        const selected = Number(await alertSelect(
                            [...labels, language.cancel],
                            language.chooseLocalSnapshot,
                        ))
                        return snapshots[selected]?.path ?? null
                    },
                    confirm: () => alertConfirm(language.restoreLocalSnapshotConfirm),
                    restart: restartNativeApp,
                    onEmpty: () => alertNormal(language.noLocalSnapshots),
                })
            } catch (error) {
                alertError(error instanceof Error ? error : String(error))
            }
        }} className="mt-2">
        {language.restoreLocalSnapshot}
    </Button>
{/if}

<Button
    onclick={async () => {
        if((await alertConfirm(language.pocketRisuImportConfirm)) && (await alertConfirm(language.backupLoadConfirm2))){
            LoadLocalBackup()
        }
    }} className="mt-2">
    {language.loadPocketRisuBackup}
</Button>

<Button
    onclick={() => {
        openSyncConflictBackups()
    }} className="mt-2">
    {language.syncConflictBackups}
</Button>

{#if forageStorage.isAccount}
    <Button
        onclick={async () => {
            loadRisuAccountBackup()
        }} className="mt-2">
        {language.loadAutoServerBackup}
    </Button>
{/if}

<Button
    onclick={async () => {
        if(await alertConfirm(language.cleanColdStorageConfirm)){
            cleanColdStorage()
        }
    }} className="mt-2">
    {language.cleanColdStorage}
</Button>

<Button
    onclick={async () => {
        if(await alertConfirm(language.backupConfirm)){
            localStorage.setItem('backup', 'save')
            
            if(isTauri || isNodeServer){
                checkDriver('savetauri')
            }
            else{
                checkDriver('save')
            }
        }
    }} className="mt-2">
    {language.savebackup}
</Button>

<Button
    onclick={async () => {
        if((await alertConfirm(language.backupLoadConfirm)) && (await alertConfirm(language.backupLoadConfirm2))){
            localStorage.setItem('backup', 'load')
            if(isTauri || isNodeServer){
                checkDriver('loadtauri')
            }
            else{
                checkDriver('load')
            }
        }
    }}
    className="mt-2">
    {language.loadbackup}
</Button>

<Button onclick={exportAsDataset} className="mt-2">
    {language.exportAsDataset}
</Button>
<div class="bg-darkbg p-3 rounded-md mb-2 flex flex-col items-start mt-2">
    <div class="w-full">
        <h1 class="text-3xl font-black min-w-0">Risu Account{#if DBState.db.account}
            <button disabled={isTauri && nativeAccountBusy} class="bg-selected p-1 text-sm font-light rounded-md hover:bg-blue-500 transition-colors float-right" onclick={async () => {
                if(isTauri){
                    if(nativeAccountBusy) return
                    await runNativeAccountOperation(() => getNativeOfficialAccountFlow().logout())
                }
                else if(DBState.db.account.useSync || forageStorage.isAccount){
                    unMigrationAccount()
                }
                DBState.db.account = undefined
            }}>{language.logout}</button>
                {#if import.meta.env.DEV}
                <button class="bg-selected p-1 text-sm font-light rounded-md hover:bg-blue-500 transition-colors float-right" onclick={async () => {
                    loginToSionyw()
                }}>{language.loginSionyw}</button>

                <button class="bg-selected p-1 text-sm font-light rounded-md hover:bg-blue-500 transition-colors float-right" onclick={async () => {
                    testSionywLogin()
                }}>TestSionyw</button>
            {/if}
        {/if}</h1>
    </div>
    {#if DBState.db.account}
        <span class="mb-4 text-textcolor2">ID: {DBState.db.account.id}</span>
        {#if isTauri}
            <Button
                disabled={nativeAccountBusy}
                onclick={async () => {
                    await runNativeAccountOperation(async () => {
                        if(!await alertConfirm('Replace local data with the official account backup?')) return
                        if(!await alertConfirm('Official snapshots do not include separate inlay payloads. Referenced image, audio, video, and signature inlays may not be restored. The app will restart after restoring the official account backup. Continue?')) return
                        try {
                            const result = await getNativeOfficialAccountFlow().restore()
                            if(result.kind === 'missing') {
                                alertNormal('No official account backup was found. Local data was not changed.')
                            }
                        } catch (error) {
                            alertError(error instanceof Error ? error : String(error))
                        }
                    })
                }} className="mt-2">
                Restore official account backup
            </Button>
            <Button
                disabled={nativeAccountBusy}
                onclick={async () => {
                    await runNativeAccountOperation(async () => {
                        if(!await alertConfirm('Overwrite the official account backup with current local data?')) return
                        try {
                            await getNativeOfficialAccountFlow().publish()
                            alertNormal('Official account backup published.')
                        } catch (error) {
                            alertError(error instanceof Error ? error : String(error))
                        }
                    })
                }} className="mt-2">
                Publish official account backup
            </Button>
        {/if}
        {#if !isTauri}
            <h1 class="text-xl font-bold mt-2">{language.googleDriveConnection}</h1>
            {#if !DBState.db.account.data.refresh_token}
                <span class="text-sm font-light mb-2 text-textcolor2">{language.googleDriveInfo}</span>
                <button class="bg-selected p-2 rounded-md hover:bg-blue-500 transition-colors" onclick={async () => {
                    const authorizationUrl = await checkDriver('reftoken')
                    if(typeof authorizationUrl === 'string') drivePopup.open(authorizationUrl)
                }}>
                    Connect to Google Drive
                </button>
            {:else}
                <span class="text-sm font-light mb-2 text-textcolor2">{language.googleDriveConnected}</span>
            {/if}
            <div class="flex items-center mt-2">
                {#if DBState.db.account.useSync || forageStorage.isAccount}
                    <Check check={true} name={language.SaveDataInAccount} onChange={(v) => {
                        if(v){
                            unMigrationAccount()
                        }
                    }}/>
                {:else}
                    <Check check={false} name={language.SaveDataInAccount} onChange={(v) => {
                        if(v){
                            localStorage.setItem('dosync', 'sync')
                            location.reload()
                        }
                    }}/>
                {/if}
            </div>
        {/if}
    {:else}
        <span>{language.notLoggedIn}</span>
        <button class="bg-selected p-2 rounded-md mt-2 hover:bg-blue-500 transition-colors" onclick={() => {
            openIframeURL = hubURL + '/hub/login'
            openIframe = true
        }}>
            Login
        </button>
    {/if}

</div>
{#if openIframe}
    <div class="fixed top-0 left-0 bg-black/50 w-full h-full flex justify-center items-center">
        <iframe bind:this={accountIframe} src={openIframeURL} title="login" class="w-full h-full">
        </iframe>
    </div>
{/if}

<!--

    My song for dear, my old friend.

    Should old aquaintance be forgot,
    and never brought to mind?
    Should old lang syne be forgot,
    and auld lang syne?

    For auld lang syne, my dear,
    for auld lang syne,
    we'll take a cup o' kindness yet,
    for auld lang syne.

-->
