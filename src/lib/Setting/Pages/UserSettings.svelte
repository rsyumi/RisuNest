<script lang="ts">
    import { language } from "src/lang";
    import { hubURL } from "src/ts/characterCards";
    import {
        loadRisuAccountBackup,
        loadRisuAccountData,
        saveRisuAccountData,
    } from "src/ts/drive/accounter";

    import { DBState } from "src/ts/stores.svelte";
    import Check from "src/lib/UI/GUI/CheckInput.svelte";
    import { alertConfirm, alertError, alertNormal } from "src/ts/alert";
    import { forageStorage } from "src/ts/globalApi.svelte";
    import { isTauri, isNodeServer } from "src/ts/platform";
    import {
        unMigrationAccount,
        accountUnmigrationBusy,
    } from "src/ts/storage/accountStorage";
    import { checkDriver } from "src/ts/drive/drive";
    import {
        SavePartialLocalBackup,
        SaveLocalBackup,
        LoadLocalBackup,
    } from "src/ts/drive/backuplocal";
    import Button from "src/lib/UI/GUI/Button.svelte";
    import { exportAsDataset } from "src/ts/storage/exportAsDataset";
    import { loginToSionyw, testSionywLogin } from "src/ts/sionyw";
    import { cleanColdStorage } from "src/ts/process/coldstorage.svelte";
    import { getNativeOfficialAccountFlow } from "src/ts/storage/sync/nativeOfficialAccountFlow";
    import {
        createHubPopupController,
        isExpectedHubMessage,
        resolveExpectedOfficialAccountMessageUrl,
    } from "src/ts/storage/officialAccountMessage";
    import {
        exportPortableBackupFromSystemPicker,
        restoreBackupFromSystemPicker,
    } from "src/ts/storage/portableBackupFileRouteProduction.svelte";
    import { onDestroy } from "svelte";
    import {
        nativeFileOperation,
        exportRisuSaveFromSystemPicker,
    } from "src/ts/storage/risuSaveFileRouteProduction.svelte";
    import {
        alertPartialDestinationWarning,
        hasPartialDestinationWarning,
    } from "src/ts/storage/risuSaveFileRoute";
    import {
        NativeFileJobActivationCommittedError,
        NativeFileJobError,
    } from "src/ts/storage/nativeFileJobs";
    import { NativeFileOperationBusyError } from "src/ts/storage/nativeFileJobManager";
    import { exportCompatibilityBackupFromSystemPicker } from "src/ts/storage/compatibleBackupFileRouteProduction.svelte";
    import { formatCompatibilityBackupReport } from "src/ts/storage/compatibleBackupReport";
    import type { NativeCompatibilityTarget } from "src/ts/storage/nativeFileJobs";
    let openIframe = $state(false);
    let openIframeURL = $state("");
    const drivePopup = createHubPopupController();
    let accountIframe = $state<HTMLIFrameElement>();
    let nativeAccountBusy = $state(false);
    let risuSaveOperation = $derived($nativeFileOperation?.kind ?? null);

    async function runNativeAccountOperation<T>(
        operation: () => Promise<T>,
    ): Promise<T | undefined> {
        if (nativeAccountBusy) return undefined;
        nativeAccountBusy = true;
        try {
            return await operation();
        } finally {
            nativeAccountBusy = false;
        }
    }

    function showRisuSaveError(error: unknown): void {
        const code =
            error && typeof error === "object" && "code" in error
                ? error.code
                : undefined;
        if (error instanceof NativeFileOperationBusyError) {
            alertError(language.risuNest.backup.fileBusy);
            return;
        }
        const blocked =
            code === "generation-active"
                ? language.risuNest.backup.generationBusy
                : code === "server-sync-busy" ||
                    code === "library-operation-busy"
                  ? language.risuNest.backup.syncBusy
                  : code === "resolve-pending-operation-first" ||
                      code === "server-status-unavailable"
                    ? language.risuNest.backup.syncUnconfirmed
                    : undefined;
        if (blocked) {
            alertError(blocked);
            return;
        }
        const partialDestinationMayRemain = hasPartialDestinationWarning(error);
        if (error instanceof DOMException && error.name === "AbortError") {
            alertPartialDestinationWarning(
                error,
                language.screenshotPartialDestinationMayRemain,
                alertError,
            );
            return;
        }
        if (error instanceof NativeFileJobActivationCommittedError) {
            alertError(language.risuSaveImportCommittedRefreshFailed);
            return;
        }
        if (
            error instanceof NativeFileJobError &&
            error.code === "revision-conflict"
        ) {
            alertError(language.risuSaveRevisionConflict);
            return;
        }
        if (
            error instanceof NativeFileJobError &&
            error.code === "source-preserved-repair-required"
        ) {
            alertError(language.risuNest.backup.sourceRepairRequired);
            return;
        }
        alertError(
            partialDestinationMayRemain
                ? `${language.risuNest.backup.actionFailed} ${language.screenshotPartialDestinationMayRemain}`
                : language.risuNest.backup.actionFailed,
        );
    }
    async function runLocalBackupOperation(
        kind: "import" | "export",
    ): Promise<void> {
        try {
            const result = isTauri
                ? kind === "import"
                    ? await restoreBackupFromSystemPicker()
                    : await exportPortableBackupFromSystemPicker()
                : kind === "import"
                  ? await LoadLocalBackup()
                  : await SaveLocalBackup();
            if (!result || ("mode" in result && result.mode === "legacy"))
                return;
            const message = result.warningCodes.includes(
                "source-preserved-repair-required",
            )
                ? language.risuNest.backup.sourcePreserved
                : kind === "import"
                  ? language.risuNest.backup.localBackupRestored
                  : language.risuNest.backup.localBackupSaved;
            alertNormal(
                result.warningCodes.includes("cleanup-failed")
                    ? `${message} ${language.risuSaveCleanupWarning}`
                    : message,
            );
        } catch (error) {
            showRisuSaveError(error);
        }
    }

    async function runCompatibleExport(
        target: NativeCompatibilityTarget,
    ): Promise<void> {
        try {
            const result =
                await exportCompatibilityBackupFromSystemPicker(target);
            if (!result) return;
            const report = result.compatibilityReport
                ? formatCompatibilityBackupReport(result.compatibilityReport, {
                      ...language.portableBackup,
                      ...language.compatibilityBackupReport,
                  })
                : "";
            const message = [language.portableBackup.saved, report]
                .filter(Boolean)
                .join("\n\n");
            alertNormal(
                result.warningCodes.includes("cleanup-failed")
                    ? `${message}\n\n${language.risuSaveCleanupWarning}`
                    : message,
            );
        } catch (error) {
            showRisuSaveError(error);
        }
    }

    onDestroy(() => {
        drivePopup.close();
    });
</script>

{#if risuSaveOperation !== null}
    <p class="text-sm opacity-70">{language.risuNest.backup.fileBusy}</p>
{/if}

<svelte:window
    onmessage={async (e) => {
        const message = e.data?.msg;
        const expectedUrl = resolveExpectedOfficialAccountMessageUrl(
            message?.type,
            hubURL,
            openIframeURL,
        );
        const expectedSource =
            message?.type === "drive"
                ? drivePopup.source
                : accountIframe?.contentWindow;
        if (!isExpectedHubMessage(e, expectedUrl, expectedSource)) return;
        if (message?.type === "drive") {
            if (!isTauri) await loadRisuAccountData();
            DBState.db.account.data.refresh_token = message.data.refresh_token;
            DBState.db.account.data.access_token = message.data.access_token;
            DBState.db.account.data.expires_in =
                message.data.expires_in * 700 + Date.now();
            if (!isTauri) await saveRisuAccountData();
            drivePopup.close();
        } else if (message?.data.vaild) {
            openIframe = false;
            const credential = {
                id: message.id,
                token: message.token,
                data: message.data,
            };
            DBState.db.account = isTauri
                ? await getNativeOfficialAccountFlow().login(credential)
                : credential;
        }
    }}
/>

<h2 class="mb-2 text-2xl font-bold mt-2">
    {language.account} & {language.files}
</h2>

<Button
    disabled={risuSaveOperation !== null}
    onclick={async () => {
        if (await alertConfirm(language.backupConfirm)) {
            await runLocalBackupOperation("export");
        }
    }}
    className="mt-2"
>
    {isTauri ? language.portableBackup.export : language.saveBackupLocal}
</Button>

{#if isTauri}
    <Button
        disabled={risuSaveOperation !== null}
        onclick={async () => {
            try {
                const result = await exportRisuSaveFromSystemPicker();
                if (result)
                    alertNormal(
                        result.warningCodes.includes("cleanup-failed")
                            ? language.risuSaveCleanupWarning
                            : language.risuSaveExportComplete,
                    );
            } catch (error) {
                showRisuSaveError(error);
            }
        }}
        className="mt-2"
    >
        {language.portableBackup.dbOnly}
    </Button>
    <Button
        disabled={risuSaveOperation !== null}
        onclick={() => runCompatibleExport("risuai")}
        className="mt-2"
    >
        {language.portableBackup.risuai}
    </Button>
    <Button
        disabled={risuSaveOperation !== null}
        onclick={() => runCompatibleExport("pocketrisu")}
        className="mt-2"
    >
        {language.portableBackup.pocket}
    </Button>
{/if}
<Button
    onclick={async () => {
        if (await alertConfirm(language.backupConfirm)) {
            SavePartialLocalBackup();
        }
    }}
    className="mt-2"
>
    {language.savePartialLocalBackup}
</Button>

<Button
    disabled={risuSaveOperation !== null}
    onclick={async () => {
        if (
            isTauri ||
            ((await alertConfirm(language.backupLoadConfirm)) &&
                (await alertConfirm(language.backupLoadConfirm2)))
        ) {
            await runLocalBackupOperation("import");
        }
    }}
    className="mt-2"
>
    {isTauri ? language.portableBackup.restore : language.loadBackupLocal}
</Button>

{#if forageStorage.isAccount}
    <Button
        onclick={async () => {
            loadRisuAccountBackup();
        }}
        className="mt-2"
    >
        {language.loadAutoServerBackup}
    </Button>
{/if}

<Button
    onclick={async () => {
        if (await alertConfirm(language.cleanColdStorageConfirm)) {
            cleanColdStorage();
        }
    }}
    className="mt-2"
>
    {language.cleanColdStorage}
</Button>

<Button
    onclick={async () => {
        if (await alertConfirm(language.backupConfirm)) {
            localStorage.setItem("backup", "save");

            if (isTauri || isNodeServer) {
                checkDriver("savetauri");
            } else {
                checkDriver("save");
            }
        }
    }}
    className="mt-2"
>
    {language.savebackup}
</Button>

<Button
    onclick={async () => {
        if (
            (await alertConfirm(language.backupLoadConfirm)) &&
            (await alertConfirm(language.backupLoadConfirm2))
        ) {
            localStorage.setItem("backup", "load");
            if (isTauri || isNodeServer) {
                checkDriver("loadtauri");
            } else {
                checkDriver("load");
            }
        }
    }}
    className="mt-2"
>
    {language.loadbackup}
</Button>

<Button onclick={exportAsDataset} className="mt-2">
    {language.exportAsDataset}
</Button>
<div class="bg-darkbg p-3 rounded-md mb-2 flex flex-col items-start mt-2">
    <div class="w-full">
        <h1 class="text-3xl font-black min-w-0">
            Risu Account{#if DBState.db.account}
                <button
                    disabled={(isTauri && nativeAccountBusy) ||
                        $accountUnmigrationBusy}
                    class="bg-selected p-1 text-sm font-light rounded-md hover:bg-blue-500 transition-colors float-right"
                    onclick={async () => {
                        if ($accountUnmigrationBusy) return;
                        if (isTauri) {
                            if (nativeAccountBusy) return;
                            await runNativeAccountOperation(() =>
                                getNativeOfficialAccountFlow().logout(),
                            );
                        } else if (
                            DBState.db.account.useSync ||
                            forageStorage.isAccount
                        ) {
                            try {
                                await unMigrationAccount();
                            } catch (error) {
                                alertError(
                                    `${language.accountUnmigration.failed}\n${error instanceof Error ? error.message : String(error)}`,
                                );
                            }
                            return;
                        }
                        DBState.db.account = undefined;
                    }}>{language.logout}</button
                >
                {#if import.meta.env.DEV}
                    <button
                        class="bg-selected p-1 text-sm font-light rounded-md hover:bg-blue-500 transition-colors float-right"
                        onclick={async () => {
                            loginToSionyw();
                        }}>{language.loginSionyw}</button
                    >

                    <button
                        class="bg-selected p-1 text-sm font-light rounded-md hover:bg-blue-500 transition-colors float-right"
                        onclick={async () => {
                            testSionywLogin();
                        }}>TestSionyw</button
                    >
                {/if}
            {/if}
        </h1>
    </div>
    {#if DBState.db.account}
        <span class="mb-4 text-textcolor2">ID: {DBState.db.account.id}</span>
        {#if !isTauri}
            <h1 class="text-xl font-bold mt-2">
                {language.googleDriveConnection}
            </h1>
            {#if !DBState.db.account.data.refresh_token}
                <span class="text-sm font-light mb-2 text-textcolor2"
                    >{language.googleDriveInfo}</span
                >
                <button
                    class="bg-selected p-2 rounded-md hover:bg-blue-500 transition-colors"
                    onclick={async () => {
                        const authorizationUrl = await checkDriver("reftoken");
                        if (typeof authorizationUrl === "string")
                            drivePopup.open(authorizationUrl);
                    }}
                >
                    Connect to Google Drive
                </button>
            {:else}
                <span class="text-sm font-light mb-2 text-textcolor2"
                    >{language.googleDriveConnected}</span
                >
            {/if}
            <fieldset
                disabled={$accountUnmigrationBusy}
                class="flex items-center mt-2"
            >
                {#if DBState.db.account.useSync || forageStorage.isAccount}
                    {#key $accountUnmigrationBusy}
                        <Check
                            check={true}
                            name={language.SaveDataInAccount}
                            onChange={async (v) => {
                                if (!v && !$accountUnmigrationBusy) {
                                    try {
                                        await unMigrationAccount();
                                    } catch (error) {
                                        alertError(
                                            `${language.accountUnmigration.failed}\n${error instanceof Error ? error.message : String(error)}`,
                                        );
                                    }
                                }
                            }}
                        />
                    {/key}
                {:else}
                    <Check
                        check={false}
                        name={language.SaveDataInAccount}
                        onChange={(v) => {
                            if (v) {
                                localStorage.setItem("dosync", "sync");
                                location.reload();
                            }
                        }}
                    />
                {/if}
            </fieldset>
        {/if}
    {:else}
        <span>{language.notLoggedIn}</span>
        <button
            class="bg-selected p-2 rounded-md mt-2 hover:bg-blue-500 transition-colors"
            onclick={() => {
                openIframeURL = hubURL + "/hub/login";
                openIframe = true;
            }}
        >
            Login
        </button>
    {/if}
</div>
{#if openIframe}
    <div
        class="fixed top-0 left-0 bg-black/50 w-full h-full flex justify-center items-center"
    >
        <iframe
            bind:this={accountIframe}
            src={openIframeURL}
            title="login"
            class="w-full h-full"
        >
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
