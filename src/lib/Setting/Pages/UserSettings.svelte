<script lang="ts">
    import { language } from "src/lang";
    import { hubURL } from "src/ts/characterCards";
    import { getDeviceMarkers } from "src/ts/storage/deviceMarkers";
    import { loadRisuAccountBackup } from "src/ts/drive/accounter";

    import { DBState } from "src/ts/stores.svelte";
    import Check from "src/lib/UI/GUI/CheckInput.svelte";
    import { alertConfirm, alertError, alertNormal } from "src/ts/alert";
    import { forageStorage } from "src/ts/globalApi.svelte";
    import { isTauri } from "src/ts/platform";
    import { openDataHealthScreen } from "src/ts/storage/dataHealthNavigation";
    import {
        unMigrationAccount,
        accountUnmigrationBusy,
    } from "src/ts/storage/accountStorage";
    import {
        SavePartialLocalBackup,
        SaveLocalBackup,
        LoadLocalBackup,
    } from "src/ts/drive/backuplocal";
    import Button from "src/lib/UI/GUI/Button.svelte";
    import { exportAsDataset } from "src/ts/storage/exportAsDataset";
    import { loginToSionyw, testSionywLogin } from "src/ts/sionyw";
    import { getNativeOfficialAccountFlow } from "src/ts/storage/sync/nativeOfficialAccountFlow";
    import {
        isExpectedHubMessage,
        resolveExpectedOfficialAccountMessageUrl,
    } from "src/ts/storage/officialAccountMessage";
    import {
        exportPortableBackupFromSystemPicker,
        restoreBackupFromSystemPicker,
    } from "src/ts/storage/portableBackupFileRouteProduction.svelte";
    import {
        nativeFileOperation,
        exportRisuSaveFromSystemPicker,
    } from "src/ts/storage/risuSaveFileRouteProduction.svelte";
    import {
        formatRisuSaveExportResult,
    } from "src/ts/storage/exportExcludedReport";
    import {
        alertPartialDestinationWarning,
        hasPartialDestinationWarning,
    } from "src/ts/storage/risuSaveFileRoute";
    import {
        NativeFileJobActivationCommittedError,
        NativeFileJobError,
    } from "src/ts/storage/nativeFileJobs";
    import {
        NativeFileOperationBusyError,
        dismissNativeFileOperationOutcome,
        nativeFileOperationOutcomeShown,
    } from "src/ts/storage/nativeFileJobManager";
    import { exportCompatibilityBackupFromSystemPicker } from "src/ts/storage/compatibleBackupFileRouteProduction.svelte";
    import { formatCompatibilityBackupReport } from "src/ts/storage/compatibleBackupReport";
    import type { NativeCompatibilityTarget } from "src/ts/storage/nativeFileJobs";
    let openIframe = $state(false);
    let openIframeURL = $state("");
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
            void offerDataHealth(
                language.risuNest.backup.sourceRepairRequired,
            );
            return;
        }
        alertError(
            partialDestinationMayRemain
                ? `${language.risuNest.backup.actionFailed} ${language.screenshotPartialDestinationMayRemain}`
                : language.risuNest.backup.actionFailed,
        );
    }
    // A failure the data check can explain offers it, instead of ending at the message.
    async function offerDataHealth(message: string): Promise<void> {
        if (!isTauri) {
            alertError(message);
            return;
        }
        if (
            await alertConfirm(
                `${message} ${language.risuNest.dataHealth.openResult}`,
            )
        )
            openDataHealthScreen();
    }

    // Exports run behind the shared progress dialog, which reports how they ended.
    function showExportError(error: unknown): void {
        if (!nativeFileOperationOutcomeShown("export")) showRisuSaveError(error);
    }

    async function runLocalBackupOperation(
        kind: "import" | "export",
    ): Promise<void> {
        if (kind === "export") dismissNativeFileOperationOutcome();
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
            if (isTauri && kind === "export") return;
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
            if (kind === "export") showExportError(error);
            else showRisuSaveError(error);
        }
    }

    async function runCompatibleExport(
        target: NativeCompatibilityTarget,
    ): Promise<void> {
        dismissNativeFileOperationOutcome();
        try {
            const result =
                await exportCompatibilityBackupFromSystemPicker(target);
            if (!result) return;
            dismissNativeFileOperationOutcome();
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
            showExportError(error);
        }
    }

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
        const expectedSource = accountIframe?.contentWindow;
        if (!isExpectedHubMessage(e, expectedUrl, expectedSource)) return;
        if (message?.data.vaild) {
            const credential = {
                id: message.id,
                token: message.token,
                data: message.data,
            };
            if (isTauri) {
                try {
                    const account = await runNativeAccountOperation(() =>
                        getNativeOfficialAccountFlow().login(credential),
                    );
                    if (!account) return;
                    DBState.db.account = account;
                } catch {
                    alertError(language.risuNest.backup.actionFailed);
                    return;
                }
            } else {
                DBState.db.account = credential;
            }
            openIframe = false;
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
            dismissNativeFileOperationOutcome();
            try {
                const result = await exportRisuSaveFromSystemPicker();
                if (!result) return;
                // The exclusion report replaces the dialog's plain completion message.
                dismissNativeFileOperationOutcome();
                alertNormal(formatRisuSaveExportResult(result));
            } catch (error) {
                showExportError(error);
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
                            try {
                                await runNativeAccountOperation(() =>
                                    getNativeOfficialAccountFlow().logout(),
                                );
                            } catch {
                                alertError(language.risuNest.backup.actionFailed);
                                return;
                            }
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
                                const markers = getDeviceMarkers();
                                markers.setItem("dosync", "sync");
                                void markers.flush().then(() => location.reload());
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
