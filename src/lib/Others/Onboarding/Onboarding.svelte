<script lang="ts">
    import { onMount } from 'svelte'
    import {
        ArrowRight,
        Check,
        ChevronLeft,
        ChevronRight,
        Cloud,
        Copy,
        FileDown,
        FolderOpen,
        Globe,
        HardDrive,
        Info,
        LoaderCircle,
        MonitorSmartphone,
        Pause,
        Server,
        Smartphone,
        TriangleAlert,
        User,
        X as XIcon,
    } from '@lucide/svelte'

    import { changeLanguage, language } from 'src/lang'
    import { alertConfirm, alertError, alertNormal } from 'src/ts/alert'
    import { hubURL } from 'src/ts/characterCards'
    import { getVersionString } from 'src/ts/globalApi.svelte'
    import { updateTextThemeAndCSS } from 'src/ts/gui/colorscheme'
    import {
        buildNativeFileJobDialogModel,
        type NativeFileJobDialogStageState,
    } from 'src/ts/gui/nativeFileJobDialogModel'
    import { isTauri, isTauriAndroid } from 'src/ts/platform'
    import { prebuiltPresets } from 'src/ts/process/templates/templates'
    import { setPreset } from 'src/ts/storage/database.svelte'
    import {
        cancelActiveNativeFileOperation,
        dismissNativeFileOperationOutcome,
        nativeFileJobHost,
        nativeFileOperation,
        nativeFileOperationOutcome,
    } from 'src/ts/storage/nativeFileJobManager'
    import {
        isExpectedHubMessage,
        resolveExpectedOfficialAccountMessageUrl,
    } from 'src/ts/storage/officialAccountMessage'
    import { restoreBackupFromSystemPicker } from 'src/ts/storage/portableBackupFileRouteProduction.svelte'
    import { importRisuSaveFromSystemPicker } from 'src/ts/storage/risuSaveFileRouteProduction.svelte'
    import { getExternalStorageBridge } from 'src/ts/storage/sync/external/bridge'
    import {
        externalJobIsActive,
        externalJobProgress,
        mergeExternalHistoryItems,
    } from 'src/ts/storage/sync/external/connection'
    import type { ExternalControllerResult } from 'src/ts/storage/sync/external/controller'
    import {
        refreshExternalStorageProductionState,
        requestExternalStorageNow,
        requestExternalStorageResolveConflict,
        requestExternalStorageRestore,
    } from 'src/ts/storage/sync/external/production'
    import type {
        ExternalConflictSummary,
        ExternalConnectionResult,
        ExternalConnectionSummary,
        ExternalHistoryItem,
        ExternalJobSummary,
    } from 'src/ts/storage/sync/external/types'
    import { getNativeOfficialAccountFlow } from 'src/ts/storage/sync/nativeOfficialAccountFlow'
    import { setAssetResidencyPolicy } from 'src/ts/storage/sync/serverAssetResidency'
    import { serverSyncError } from 'src/ts/storage/sync/serverSync'
    import {
        connectServerSync,
        serverSyncHostLabel,
        serverSyncProgressView,
        type ServerSyncConnectRequest,
    } from 'src/ts/storage/sync/serverSyncConnectFlow'
    import { getServerSyncController } from 'src/ts/storage/sync/serverSyncProduction'
    import ConnectionForm from 'src/lib/Setting/ExternalStorage/ConnectionForm.svelte'
    import {
        externalConnectionTitle,
        externalErrorMessage,
        externalStorageStrings,
    } from 'src/lib/Setting/ExternalStorage/strings'
    import ServerSyncConnect from 'src/lib/Setting/ServerSync/ServerSyncConnect.svelte'
    import { DBState } from 'src/ts/stores.svelte'

    import {
        INITIAL_ONBOARDING_FLOW,
        accountRestoreApplied,
        goToOnboardingState,
        onboardingStep,
        onboardingSummary,
        type OnboardingState,
    } from './onboardingFlow'
    import {
        externalOnboardingAction,
        externalOnboardingConflictStep,
        externalOnboardingRestorable,
        externalOnboardingRestoreAreas,
        externalOnboardingRestoreRestarts,
        externalOnboardingSyncOutcome,
    } from './externalStorageOnboardingFlow'
    import { onboardingHold } from './onboardingGate'
    import { observeOnboardingWeave } from './onboardingWeave'
    import { serverSyncOnboardingOutcome } from './serverSyncOnboardingFlow'

    const UI_LANGUAGES = [
        { value: 'de', label: 'Deutsch' },
        { value: 'en', label: 'English' },
        { value: 'ko', label: '한국어' },
        { value: 'cn', label: '中文' },
        { value: 'zh-Hant', label: '中文(繁體)' },
        { value: 'vi', label: 'Tiếng Việt' },
    ]

    // `language` is a plain module binding, so a change has to be announced for
    // the markup to read the new strings.
    let languageRevision = $state(0)
    const strings = $derived.by(() => {
        void languageRevision
        return language
    })
    const t = $derived(strings.risuNest.onboarding)

    {
        const browserLangShort = navigator.language.split('-')[0]
        const usableLangs = ['de', 'en', 'ko', 'cn', 'vi', 'zh-Hant']
        if (usableLangs.includes(browserLangShort)) {
            changeLanguage(browserLangShort)
            DBState.db.language = browserLangShort
        }
    }

    import { serverSyncScreenRequest } from 'src/ts/storage/sync/serverSyncDeepLink'
    let lastServerRequest: unknown
    $effect(() => {
        if (
            !isTauri ||
            !$serverSyncScreenRequest ||
            lastServerRequest === $serverSyncScreenRequest
        )
            return
        lastServerRequest = $serverSyncScreenRequest
        flow = goToOnboardingState(flow, 'sync-hub')
    })

    let flow = $state(INITIAL_ONBOARDING_FLOW)
    const step = $derived(onboardingStep(flow.state))

    let weaveCanvas = $state<HTMLCanvasElement | undefined>()
    let importBusy = $state(false)
    let accountBusy = $state(false)
    let loginOpen = $state(false)
    let loginUrl = $state('')
    let loginFrame = $state<HTMLIFrameElement | undefined>()

    // The shared import dialog's view model, drawn in this panel instead of
    // the popup while the onboarding is up. `now` only feeds the elapsed time.
    let now = $state(Date.now())
    const job = $derived(
        buildNativeFileJobDialogModel(
            $nativeFileOperation,
            $nativeFileOperationOutcome,
            now,
        ),
    )
    const jobShown = $derived(job.open && !job.compact)
    const jobTicking = $derived(jobShown && job.terminal === null)
    let jobDetailsOpen = $state(false)
    let jobDetailsCopied = $state(false)

    type StageRow = {
        stage: string
        label: string
        state: NativeFileJobDialogStageState
        detail: string
    }

    // The sync server screen. The shared connect part collects the code and
    // the check; this component runs the connection and draws the attempt
    // with the same progress panel as a backup import.
    const syncController = isTauri ? getServerSyncController() : undefined
    let syncSnapshot = $state(syncController?.snapshot())
    let hubStage = $state<'code' | 'review'>('code')
    let hubStarted = $state(false)
    let hubConnecting = $state(false)
    let hubPausing = $state(false)
    let hubError = $state('')
    let hubKey = $state(0)
    let hubServer = $state<{ endpoint: string; libraryId: string; deviceId: string }>()
    // Kept only while this screen is up, so a failed registration can be retried.
    let hubRequest: ServerSyncConnectRequest | undefined
    const s = $derived(strings.risuNest.serverSync)

    // The external storage screen. The shared connection form collects the
    // recovery key; this component runs what the repository is opened for.
    const externalBridge = isTauri ? getExternalStorageBridge() : undefined
    /** How many empty history pages one request reads past before stopping. */
    const EXTERNAL_HISTORY_STEPS = 4
    const ex = $derived(t.external)
    const externalStrings = $derived(externalStorageStrings(DBState.db.language))
    let externalConnection = $state<ExternalConnectionSummary | undefined>()
    let externalStage = $state<'connect' | 'choose' | 'working' | 'conflict' | 'error'>('connect')
    let externalWorking = $state(false)
    let externalError = $state('')
    let externalHistory = $state<ExternalHistoryItem[]>([])
    let externalCursor = $state<string | undefined>()
    let externalSelected = $state('')
    let externalConflict = $state<ExternalConflictSummary | undefined>()
    let externalJob = $state<ExternalJobSummary | undefined>()
    let externalKey = $state(0)
    const externalRestorable = $derived(externalOnboardingRestorable(externalHistory))
    const externalPercent = $derived.by(() => {
        const progress = externalJob ? externalJobProgress(externalJob) : null
        return progress === null ? null : Math.round(progress * 100)
    })
    const hubOutcome = $derived(
        hubStarted && syncSnapshot ? serverSyncOnboardingOutcome(syncSnapshot) : undefined,
    )
    const hubSyncing = $derived(hubStarted && (hubConnecting || hubOutcome === 'syncing'))
    const hubView = $derived(
        syncSnapshot?.running ? serverSyncProgressView(syncSnapshot, s, now) : undefined,
    )
    const hubConflict = $derived(
        syncSnapshot?.result?.phase === 'conflict' ? syncSnapshot.result : undefined,
    )
    const hubErrorCode = $derived(hubError || syncSnapshot?.error || '')
    const hubTicking = $derived(hubSyncing && Boolean(syncSnapshot?.running))

    $effect(() => {
        if (!hubStarted || hubConnecting || !hubOutcome) return
        if (hubOutcome === 'complete' || hubOutcome === 'paused') {
            resetHub()
            goTo('done')
        }
    })

    // The native side owns the job; this reads its progress while it runs.
    $effect(() => {
        if (externalStage !== 'working' || !externalBridge) return
        let stopped = false
        const read = async () => {
            try {
                const state = await externalBridge.getState()
                if (stopped) return
                externalJob = state.jobs.find(
                    (job) => job.connectionId === externalConnection?.id && externalJobIsActive(job),
                )
            } catch {
                // The screen keeps its own error; a progress read may fail.
            }
        }
        void read()
        const timer = setInterval(() => void read(), 1200)
        return () => {
            stopped = true
            clearInterval(timer)
            externalJob = undefined
        }
    })

    $effect(() => {
        if (!jobTicking && !hubTicking) return
        now = Date.now()
        const timer = setInterval(() => {
            now = Date.now()
        }, 1000)
        return () => clearInterval(timer)
    })

    $effect(() => {
        if (jobTicking) {
            jobDetailsOpen = false
            jobDetailsCopied = false
        }
    })

    onMount(() => {
        // A restored database carries its own `didFirstSetup`; the hold keeps
        // this screen up until the reader presses start.
        onboardingHold.set(true)
        // Backup restores report through the shared operation stores; this
        // panel draws them while it is up, so the popup stays closed.
        nativeFileJobHost.set('onboarding')
        const stopWeave = weaveCanvas
            ? observeOnboardingWeave(weaveCanvas)
            : () => {}
        const stopSync = syncController
            ? syncController.subscribe((value) => {
                  syncSnapshot = value
              })
            : () => {}
        return () => {
            stopWeave()
            stopSync()
            hubRequest = undefined
            nativeFileJobHost.set('dialog')
            onboardingHold.set(false)
        }
    })

    function goTo(state: OnboardingState): void {
        flow = goToOnboardingState(flow, state)
    }

    function setLanguage(value: string): void {
        DBState.db.language = value
        changeLanguage(value)
        languageRevision += 1
    }

    /** The defaults a reader who skips setup would otherwise have to choose. */
    function startFresh(): void {
        // Data that arrived outside this screen, such as a backup opened from
        // a file manager, already carries its own settings.
        if (DBState.db.didFirstSetup) {
            flow = goToOnboardingState(flow, 'done', 'import')
            return
        }
        DBState.db = setPreset(DBState.db, prebuiltPresets.OAI2)
        DBState.db.textTheme = 'highcontrast'
        updateTextThemeAndCSS()
        DBState.db.maxContext = 16000
        DBState.db.maxResponse = 1000
        DBState.db.claudeCachingExperimental = true
        flow = goToOnboardingState(flow, 'done', 'fresh')
    }

    function finish(): void {
        DBState.db.didFirstSetup = true
        dismissNativeFileOperationOutcome()
        onboardingHold.set(false)
    }

    /**
     * Both import routes report through the shared operation stores. The job
     * panel draws the progress and outcome, and the reader moves on from it.
     * A first run has nothing to lose, so the native route skips the
     * replacement confirmation it shows in the settings.
     */
    async function runImport(): Promise<void> {
        if (importBusy) return
        importBusy = true
        try {
            if (isTauri) await restoreBackupFromSystemPicker({ firstRun: true })
            else await importRisuSaveFromSystemPicker()
        } catch {
            // Preflight failures happen before the shared operation panel exists.
            alertError(strings.risuNest.backup.actionFailed)
        } finally {
            importBusy = false
        }
    }

    function continueAfterImport(): void {
        dismissNativeFileOperationOutcome()
        flow = goToOnboardingState(flow, 'done', 'import')
    }

    async function copyJobDetails(): Promise<void> {
        const details = job.terminal?.details
        if (!details) return
        try {
            await navigator.clipboard.writeText(details)
            jobDetailsCopied = true
            setTimeout(() => {
                jobDetailsCopied = false
            }, 1500)
        } catch {
            jobDetailsCopied = false
        }
    }

    function resetHub(): void {
        hubRequest = undefined
        hubServer = undefined
        hubStarted = false
        hubError = ''
        hubStage = 'code'
        hubKey += 1
    }

    async function startHub(request: ServerSyncConnectRequest): Promise<void> {
        if (!syncController || hubConnecting) return
        hubRequest = request
        hubServer = {
            endpoint: request.config.endpoint,
            libraryId: request.config.libraryId,
            deviceId: request.config.deviceId,
        }
        hubStarted = true
        hubConnecting = true
        hubError = ''
        try {
            await connectServerSync(syncController, setAssetResidencyPolicy, request)
        } catch (cause) {
            hubError = serverSyncError(cause).code
        } finally {
            hubConnecting = false
        }
    }

    /** Bound already: run the attempt again. Not bound: register again. */
    async function retryHub(): Promise<void> {
        if (!syncController || hubConnecting) return
        if (!syncSnapshot?.status?.configured) {
            if (hubRequest) await startHub(hubRequest)
            else resetHub()
            return
        }
        hubConnecting = true
        hubError = ''
        try {
            await syncController.synchronize()
        } catch (cause) {
            hubError = serverSyncError(cause).code
        } finally {
            hubConnecting = false
        }
    }

    async function resolveHub(resolution: 'keep-local' | 'keep-remote'): Promise<void> {
        if (!syncController || !hubConflict || hubConnecting) return
        hubConnecting = true
        hubError = ''
        try {
            await syncController.synchronize({
                resolution,
                expectedRevision: hubConflict.localRevision,
                expectedHead: hubConflict.head,
            })
        } catch (cause) {
            hubError = serverSyncError(cause).code
        } finally {
            hubConnecting = false
        }
    }

    async function pauseHub(): Promise<void> {
        if (!syncController || hubPausing) return
        hubPausing = true
        try {
            await syncController.pause()
            await syncController.waitForIdle()
        } finally {
            hubPausing = false
        }
    }

    function externalWhen(value: string): string {
        const time = Number(value)
        return Number.isFinite(time) ? new Date(time).toLocaleString() : '—'
    }

    function externalSize(value?: string): string {
        if (!value) return ''
        const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
        let amount = Number(value)
        if (!Number.isFinite(amount)) return ''
        let index = 0
        while (index < units.length - 1 && amount >= 1024) {
            amount /= 1024
            index += 1
        }
        return `${index === 0 ? amount : Math.round(amount * 10) / 10} ${units[index]}`
    }

    function resetExternal(): void {
        externalConnection = undefined
        externalStage = 'connect'
        externalWorking = false
        externalError = ''
        externalHistory = []
        externalCursor = undefined
        externalSelected = ''
        externalConflict = undefined
        externalJob = undefined
        externalKey += 1
    }

    async function onExternalConnected(result: ExternalConnectionResult): Promise<void> {
        externalConnection = result.connection
        externalError = ''
        externalStage = 'choose'
        try {
            await refreshExternalStorageProductionState()
        } catch (cause) {
            externalFailure(cause)
            return
        }
        if (externalOnboardingAction(result.connection) === 'restore') {
            await loadExternalHistory(false)
        }
    }

    async function loadExternalHistory(append: boolean): Promise<void> {
        const connection = externalConnection
        if (!externalBridge || !connection || externalWorking) return
        externalWorking = true
        try {
            let items = append ? externalHistory : []
            let cursor = append ? externalCursor : undefined
            // The first page holds kept and conflict copies only, so a
            // repository that just holds backups answers it with nothing.
            for (let step = 0; step < EXTERNAL_HISTORY_STEPS; step += 1) {
                const page = await externalBridge.listHistory(connection.id, cursor)
                items = mergeExternalHistoryItems(items, page.items)
                cursor = page.nextCursor
                if (page.items.length > 0 || !cursor) break
            }
            externalHistory = items
            externalCursor = cursor
            const restorable = externalOnboardingRestorable(items)
            if (!restorable.some((item) => item.id === externalSelected)) {
                externalSelected = restorable[0]?.id ?? ''
            }
            externalError = ''
        } catch (cause) {
            externalFailure(cause)
        } finally {
            externalWorking = false
        }
    }

    function externalFailure(cause: unknown): void {
        externalError = externalErrorMessage(externalStrings, cause)
        externalStage = 'error'
    }

    /** Where a finished synchronization attempt leaves the screen. */
    async function readExternalSync(result: ExternalControllerResult): Promise<void> {
        const outcome = externalOnboardingSyncOutcome(result)
        if (outcome === 'complete') {
            flow = goToOnboardingState(flow, 'done', 'external')
            return
        }
        if (outcome === 'conflict') {
            await loadExternalConflict()
            return
        }
        externalFailure(
            result.kind === 'blocked'
                ? result.error ?? { code: result.reason }
                : { code: 'cancelled' },
        )
    }

    async function startExternalSync(): Promise<void> {
        const connection = externalConnection
        if (!externalBridge || !connection || externalWorking) return
        externalWorking = true
        externalError = ''
        externalStage = 'working'
        try {
            const state = await externalBridge.getState()
            if (
                state.selection.kind !== 'external'
                || state.selection.connectionId !== connection.id
            ) {
                await externalBridge.setSyncTarget(
                    connection.id,
                    state.selection.selectionEpoch,
                )
                await refreshExternalStorageProductionState()
            }
            await readExternalSync(await requestExternalStorageNow(connection.id, 'sync'))
        } catch (cause) {
            externalFailure(cause)
        } finally {
            externalWorking = false
        }
    }

    async function loadExternalConflict(): Promise<void> {
        const connection = externalConnection
        if (!externalBridge || !connection) return
        try {
            externalConflict = (await externalBridge.listConflicts(undefined, 50)).conflicts
                .find(conflict => conflict.connectionId === connection.id)
            externalStage = externalConflict ? 'conflict' : 'error'
        } catch (cause) {
            externalFailure(cause)
        }
    }

    /**
     * Only the repository side is offered. Keeping this device would publish
     * the starting library of a first run to every other device.
     */
    async function takeExternalRepository(): Promise<void> {
        const connection = externalConnection
        const conflict = externalConflict
        if (!connection || !conflict || externalWorking) return
        externalWorking = true
        externalError = ''
        externalStage = 'working'
        try {
            if (externalOnboardingConflictStep(conflict) === 'receive-repository') {
                await readExternalSync(await requestExternalStorageNow(connection.id, 'sync'))
                return
            }
            await requestExternalStorageResolveConflict(connection.id, conflict.id, 'remote')
            flow = goToOnboardingState(flow, 'done', 'external')
        } catch (cause) {
            externalFailure(cause)
        } finally {
            externalWorking = false
        }
    }

    async function restoreExternalBackup(): Promise<void> {
        const connection = externalConnection
        const item = externalRestorable.find((entry) => entry.id === externalSelected)
        if (!connection || !item || externalWorking) return
        externalWorking = true
        externalError = ''
        externalStage = 'working'
        try {
            await requestExternalStorageRestore(
                connection.id,
                item.id,
                externalOnboardingRestoreAreas(item),
            )
            flow = goToOnboardingState(flow, 'done', 'external')
        } catch (cause) {
            externalFailure(cause)
        } finally {
            externalWorking = false
        }
    }

    function openAccountLogin(): void {
        loginUrl = hubURL + '/hub/login'
        loginOpen = true
    }

    function closeAccountLogin(): void {
        loginOpen = false
    }

    async function restoreAccountBackup(): Promise<void> {
        if (accountBusy) return
        accountBusy = true
        let restarting = false
        try {
            // The account snapshot, the same one the backup settings restore.
            // The versioned /hub/backup list is a rollback tool for readers
            // already running on account storage, not a way onto a new device.
            const result = await getNativeOfficialAccountFlow().restore()
            if (result.kind === 'missing') {
                alertNormal(strings.risuNest.backup.officialMissing)
                return
            }
            if (!accountRestoreApplied(result.kind)) {
                alertNormal(t.accountFound.notRestored)
                return
            }
            // An activated snapshot restarts the app, so the button stays busy
            // until the process goes.
            restarting = true
        } catch {
            alertError(strings.risuNest.backup.actionFailed)
        } finally {
            if (!restarting) accountBusy = false
        }
    }
</script>

<svelte:window
    onkeydown={(event) => {
        if (loginOpen && event.key === 'Escape') closeAccountLogin()
    }}
    onmessage={async (event) => {
        if (!loginOpen) return
        const message = event.data?.msg
        const expectedUrl = resolveExpectedOfficialAccountMessageUrl(
            message?.type,
            hubURL,
            loginUrl,
        )
        if (
            !isExpectedHubMessage(event, expectedUrl, loginFrame?.contentWindow)
        )
            return
        if (!message?.data?.vaild) return
        loginOpen = false
        const credential = {
            id: message.id,
            token: message.token,
            data: message.data,
        }
        try {
            DBState.db.account =
                await getNativeOfficialAccountFlow().login(credential)
        } catch {
            alertError(strings.risuNest.backup.actionFailed)
            return
        }
        flow = goToOnboardingState(flow, 'sync-account-found', 'account')
    }}
/>

{#snippet steps(place: 'top' | 'bottom')}
    <ol class="steps {place}">
        {#each [t.stepStart, t.stepData, t.stepDone] as label, index}
            <li class:on={step === index + 1} class:past={step > index + 1}>
                <i></i><span>{label}</span>
            </li>
        {/each}
    </ol>
{/snippet}

{#snippet back(target: OnboardingState, label: string)}
    <button class="back" type="button" onclick={() => goTo(target)}>
        <ChevronLeft />{label}
    </button>
{/snippet}

{#snippet hubServerChip()}
    {@const server = syncSnapshot?.status?.configured
        ? {
              endpoint: syncSnapshot.status.endpoint ?? '',
              libraryId: syncSnapshot.status.libraryId ?? '',
              deviceId: syncSnapshot.status.deviceId ?? '',
          }
        : hubServer}
    {#if server}
        <div class="file">
            <Server />
            <span class="name">{serverSyncHostLabel(server.endpoint)}</span>
            <span class="dim">{server.libraryId} · {server.deviceId}</span>
            <span class="tag">{t.hub.serverTag}</span>
        </div>
    {/if}
{/snippet}

{#snippet externalRepositoryChip()}
    {#if externalConnection}
        <div class="file">
            <HardDrive />
            <span class="name"
                >{externalConnectionTitle(externalStrings, externalConnection)}</span
            >
            <span class="dim">{externalConnection.endpoint.repositoryHint}</span>
        </div>
    {/if}
{/snippet}

{#snippet bar(percent: number | null, label: string)}
    <div
        class="track"
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent ?? undefined}
    >
        <div
            class="fill"
            class:pulse={percent === null}
            style:width={percent === null ? '100%' : `${percent}%`}
        ></div>
    </div>
{/snippet}

{#snippet stageList(rows: StageRow[])}
    {#if rows.length > 0}
        <ol class="stages">
            {#each rows as row (row.stage)}
                <li data-state={row.state}>
                    <span class="mark">
                        {#if row.state === 'done'}<Check />
                        {:else if row.state === 'active'}<LoaderCircle />
                        {:else if row.state === 'stopped'}<XIcon />{/if}
                    </span>
                    <span class="label">{row.label}</span>
                    {#if row.detail}<span class="detail">{row.detail}</span
                        >{/if}
                </li>
            {/each}
        </ol>
    {/if}
{/snippet}

<div class="onb-root" class:keep-all={DBState.db.language === 'ko'}>
    <div class="onb">
        <aside class="brand">
            <canvas bind:this={weaveCanvas} aria-hidden="true"></canvas>
            <div class="brand-top">
                <img
                    class="wm"
                    src="/wordmark-transparent.svg"
                    alt="RisuNest"
                />
                {@render steps('top')}
            </div>
            <div class="brand-copy">
                <p class="eyebrow">{t.eyebrow}</p>
                <h2>{t.brandTitle}</h2>
                <p class="desc">{t.brandDesc}</p>
            </div>
            {@render steps('bottom')}
        </aside>

        <section class="panel">
            {#if jobShown}
                <div class="panel-in">
                    <h1>{job.title}</h1>
                    {#if job.terminal === null}
                        <p class="lead">{t.import.warning}</p>
                    {/if}
                    {#if job.sourceName || job.subtitle}
                        <div class="file">
                            <FileDown />
                            <span class="name">{job.sourceName}</span>
                            {#if job.sourceSize}<span class="dim"
                                    >{job.sourceSize}</span
                                >{/if}
                            {#if job.subtitle}<span class="tag"
                                    >{job.subtitle}</span
                                >{/if}
                        </div>
                    {/if}
                    {@render bar(
                        job.indeterminate ? null : (job.overallPercent ?? 0),
                        strings.risuNest.importDialog.titleImport,
                    )}
                    <p class="meta">
                        <span
                            >{job.overallPercent !== null
                                ? `${job.overallPercent}%`
                                : job.terminal
                                  ? ''
                                  : strings.risuNest.importDialog
                                        .preparing}</span
                        >
                        <span>{job.overallText}</span>
                        <span class="dim">{job.elapsed}</span>
                    </p>
                    {#if job.terminal}
                        <p
                            class="result"
                            class:succeeded={job.terminal.state === 'succeeded'}
                            class:failed={job.terminal.state === 'failed'}
                            class:cancelled={job.terminal.state === 'cancelled'}
                            role="status"
                        >
                            {job.terminal.summary}
                        </p>
                        {#if job.terminal.reason}<p class="reason">
                                {job.terminal.reason}
                            </p>{/if}
                    {/if}
                    {@render stageList(job.stages)}
                    {#if job.currentItem}<p class="item">
                            {job.currentItem}
                        </p>{/if}
                    {#if job.counters.length > 0}
                        <dl class="counts" style:--cards={job.counters.length}>
                            {#each job.counters as counter (counter.key)}
                                <div>
                                    <dt>{counter.label}</dt>
                                    <dd>{counter.value}</dd>
                                </div>
                            {/each}
                        </dl>
                    {/if}
                    {#if job.warnings.length > 0}
                        <ul class="warnings">
                            {#each job.warnings as warning}<li>
                                    {warning}
                                </li>{/each}
                        </ul>
                    {/if}
                    {#if job.terminal?.details}
                        <div class="details">
                            <button
                                class="btn ghost"
                                type="button"
                                onclick={() => {
                                    jobDetailsOpen = !jobDetailsOpen
                                }}
                            >
                                {strings.risuNest.importDialog.errorDetails}
                            </button>
                            {#if jobDetailsOpen}
                                <div class="details-body">
                                    <button
                                        class="copy"
                                        type="button"
                                        title={jobDetailsCopied
                                            ? strings.risuNest.importDialog
                                                  .copied
                                            : strings.risuNest.importDialog
                                                  .copyDetails}
                                        aria-label={strings.risuNest
                                            .importDialog.copyDetails}
                                        onclick={copyJobDetails}
                                    >
                                        {#if jobDetailsCopied}<Check
                                            />{:else}<Copy />{/if}
                                    </button>
                                    <pre>{job.terminal.details}</pre>
                                </div>
                            {/if}
                        </div>
                    {/if}
                    <div class="actions">
                        {#if job.cancelVisible}
                            <button
                                class="btn ghost"
                                type="button"
                                disabled={!job.cancelEnabled}
                                onclick={cancelActiveNativeFileOperation}
                            >
                                {job.cancelLabel}
                            </button>
                            {#if job.cancelNote}<span class="dim"
                                    >{job.cancelNote}</span
                                >{/if}
                        {/if}
                        {#if job.closeVisible}
                            {#if job.terminal?.state === 'succeeded'}
                                <button
                                    class="btn primary"
                                    type="button"
                                    onclick={continueAfterImport}
                                >
                                    {t.import.next}<ArrowRight />
                                </button>
                            {:else}
                                <button
                                    class="btn"
                                    type="button"
                                    onclick={dismissNativeFileOperationOutcome}
                                >
                                    {strings.risuNest.importDialog.close}
                                </button>
                            {/if}
                        {/if}
                    </div>
                </div>
            {:else}
                {#key flow.state}
                    <div class="panel-in">
                        {#if flow.state === 'home'}
                            <h1>{t.home.title}</h1>
                            <p class="lead">{t.home.lead}</p>
                            <div class="rows">
                                <button
                                    class="row primary"
                                    type="button"
                                    onclick={startFresh}
                                >
                                    <span class="ic"><ArrowRight /></span>
                                    <span class="tx"
                                        ><b>{t.home.freshTitle}</b><small
                                            >{t.home.freshDesc}</small
                                        ></span
                                    >
                                    <span class="chev"><ChevronRight /></span>
                                </button>
                                <button
                                    class="row"
                                    type="button"
                                    onclick={() => goTo('import')}
                                >
                                    <span class="ic"><FileDown /></span>
                                    <span class="tx"
                                        ><b>{t.home.importTitle}</b><small
                                            >{t.home.importDesc}</small
                                        ></span
                                    >
                                    <span class="chev"><ChevronRight /></span>
                                </button>
                                <!-- Every sync route needs the native transports, so the web
                                 build would open this on an empty screen. -->
                                {#if isTauri}
                                    <button
                                        class="row"
                                        type="button"
                                        onclick={() => goTo('sync')}
                                    >
                                        <span class="ic"
                                            ><MonitorSmartphone /></span
                                        >
                                        <span class="tx"
                                            ><b>{t.home.syncTitle}</b><small
                                                >{t.home.syncDesc}</small
                                            ></span
                                        >
                                        <span class="chev"
                                            ><ChevronRight /></span
                                        >
                                    </button>
                                {/if}
                            </div>
                            <footer class="panel-foot">
                                <label class="pill">
                                    <Globe />
                                    <span class="sr-only">{t.language}</span>
                                    <select
                                        value={DBState.db.language}
                                        onchange={(event) =>
                                            setLanguage(
                                                event.currentTarget.value,
                                            )}
                                    >
                                        {#each UI_LANGUAGES as option}
                                            <option value={option.value}
                                                >{option.label}</option
                                            >
                                        {/each}
                                    </select>
                                </label>
                                <span>RisuNest {getVersionString()}</span>
                            </footer>
                        {:else if flow.state === 'import'}
                            {@render back('home', t.backHome)}
                            <h1>{t.import.title}</h1>
                            <p class="lead">{t.import.lead}</p>
                            <div class="drop">
                                <span class="ic"><FileDown /></span>
                                <b>{t.import.dropTitle}</b>
                                <button
                                    class="btn primary"
                                    type="button"
                                    disabled={importBusy}
                                    onclick={runImport}
                                >
                                    <FolderOpen />{t.import.choose}
                                </button>
                            </div>
                            {#if isTauriAndroid}
                                <p class="hint">
                                    <Smartphone /><span
                                        >{t.import.hintAndroid}</span
                                    >
                                </p>
                            {/if}
                            <div class="detect">
                                <span class="ic"><Info /></span>
                                <div>
                                    <b>{t.import.supportTitle}</b><small
                                        >{t.import.supportDesc}</small
                                    >
                                </div>
                            </div>
                            <p class="note warn">
                                <TriangleAlert /><span>{t.import.warning}</span>
                            </p>
                        {:else if flow.state === 'sync'}
                            {@render back('home', t.backHome)}
                            <h1>{t.sync.title}</h1>
                            <p class="lead">{t.sync.lead}</p>
                            <div class="rows">
                                <button
                                    class="row"
                                    type="button"
                                    onclick={() => goTo('sync-hub')}
                                >
                                    <span class="ic"><Server /></span>
                                    <span class="tx"
                                        ><b>{t.sync.hubTitle}</b><small
                                            >{t.sync.hubDesc}</small
                                        ></span
                                    >
                                    <span class="chev"><ChevronRight /></span>
                                </button>
                                <button
                                    class="row"
                                    type="button"
                                    onclick={() => goTo('sync-external')}
                                >
                                    <span class="ic"><HardDrive /></span>
                                    <span class="tx"
                                        ><b>{t.sync.externalTitle}</b><small
                                            >{t.sync.externalDesc}</small
                                        ></span
                                    >
                                    <span class="chev"><ChevronRight /></span>
                                </button>
                                <button
                                    class="row"
                                    type="button"
                                    onclick={() => goTo('sync-account')}
                                >
                                    <span class="ic"><Cloud /></span>
                                    <span class="tx"
                                        ><b>{t.sync.accountTitle}</b><small
                                            >{t.sync.accountDesc}</small
                                        ></span
                                    >
                                    <span class="chev"><ChevronRight /></span>
                                </button>
                            </div>
                        {:else if flow.state === 'sync-hub'}
                            {#if !isTauri}
                                {@render back('sync', t.back)}
                                <h1>{t.hub.title}</h1>
                                <p class="lead">{t.hub.stepLink}</p>
                            {:else if hubSyncing}
                                <h1>{s.running}</h1>
                                <p class="lead">{t.hub.syncingLead}</p>
                                {@render hubServerChip()}
                                {@render bar(hubView?.percent ?? null, s.running)}
                                <p class="meta">
                                    <span
                                        >{hubView?.percent !== null &&
                                        hubView?.percent !== undefined
                                            ? `${hubView.percent}%`
                                            : strings.risuNest.importDialog
                                                  .preparing}</span
                                    >
                                    <span>{hubView?.current ?? s.running}</span>
                                    <span class="dim">{hubView?.elapsed ?? ''}</span>
                                </p>
                                {#if hubView}
                                    {@render stageList(hubView.stages)}
                                    <dl
                                        class="counts"
                                        style:--cards={hubView.counters.length}
                                    >
                                        {#each hubView.counters as counter (counter.key)}
                                            <div>
                                                <dt>{counter.label}</dt>
                                                <dd>{counter.value}</dd>
                                            </div>
                                        {/each}
                                    </dl>
                                {/if}
                                <div class="actions">
                                    <button
                                        class="btn ghost"
                                        type="button"
                                        disabled={hubPausing ||
                                            !syncSnapshot?.running ||
                                            syncSnapshot.paused}
                                        onclick={() => void pauseHub()}
                                    >
                                        <Pause />{s.pause}
                                    </button>
                                    <span class="dim">{t.hub.pauseNote}</span>
                                </div>
                            {:else if hubStarted && hubOutcome === 'conflict' && hubConflict}
                                <h1>{t.hub.title}</h1>
                                {@render hubServerChip()}
                                <p class="result failed" role="status">
                                    {s.conflictCount.replace(
                                        '{0}',
                                        String(hubConflict.conflictCount),
                                    )}
                                </p>
                                <p class="reason">{s.conflictHelp}</p>
                                <div class="actions">
                                    <button
                                        class="btn primary"
                                        type="button"
                                        onclick={() => void resolveHub('keep-local')}
                                    >
                                        {s.keepLocal}
                                    </button>
                                    <button
                                        class="btn"
                                        type="button"
                                        onclick={() => void resolveHub('keep-remote')}
                                    >
                                        {s.keepRemote}
                                    </button>
                                </div>
                            {:else if hubStarted}
                                <button class="back" type="button" onclick={resetHub}>
                                    <ChevronLeft />{s.otherCode}
                                </button>
                                <h1>{t.hub.title}</h1>
                                {@render hubServerChip()}
                                <p class="result failed" role="status">
                                    {t.hub.errorSummary}
                                </p>
                                <p class="reason">
                                    {hubOutcome === 'pending'
                                        ? t.hub.pendingReason
                                        : t.hub.errorReason}
                                    {#if hubErrorCode && hubErrorCode !== 'cancelled'}<span
                                            class="dim">({hubErrorCode})</span
                                        >{/if}
                                </p>
                                <div class="actions">
                                    <button
                                        class="btn primary"
                                        type="button"
                                        onclick={() => void retryHub()}
                                    >
                                        {t.hub.retry}
                                    </button>
                                    <button class="btn ghost" type="button" onclick={resetHub}>
                                        {s.otherCode}
                                    </button>
                                    <button
                                        class="btn ghost"
                                        type="button"
                                        onclick={() => {
                                            resetHub()
                                            goTo('home')
                                        }}
                                    >
                                        {t.backHome}
                                    </button>
                                </div>
                            {:else}
                                {#if hubStage === 'review'}
                                    <button class="back" type="button" onclick={resetHub}>
                                        <ChevronLeft />{s.otherCode}
                                    </button>
                                    <h1>{s.reviewTitle}</h1>
                                    <p class="lead">{t.hub.reviewLead}</p>
                                {:else}
                                    {@render back('sync', t.back)}
                                    <h1>{t.hub.title}</h1>
                                    <p class="lead">
                                        {isTauriAndroid ? t.hub.leadScan : t.hub.lead}
                                    </p>
                                    <ol class="howto">
                                        <li>{t.hub.stepLink}</li>
                                        <li>{t.hub.stepPaste}</li>
                                        <li>{t.hub.stepReview}</li>
                                    </ol>
                                {/if}
                                {#key hubKey}
                                    <ServerSyncConnect
                                        bind:stage={hubStage}
                                        initialNavigation={$serverSyncScreenRequest}
                                        tone="onboarding"
                                        busy={hubConnecting}
                                        onSubmit={(request) => void startHub(request)}
                                    />
                                {/key}
                                {#if isTauriAndroid && hubStage === 'code'}
                                    <p class="hint spaced">
                                        <Smartphone /><span>{t.hub.linkHint}</span>
                                    </p>
                                {/if}
                            {/if}
                        {:else if flow.state === 'sync-external'}
                            {#if !isTauri}
                                {@render back('sync', t.back)}
                                <h1>{ex.title}</h1>
                                <p class="lead">{ex.unsupported}</p>
                            {:else if externalStage === 'working'}
                                <h1>
                                    {externalConnection
                                        && externalOnboardingAction(externalConnection) === 'sync'
                                        ? ex.syncTitle
                                        : ex.restoring}
                                </h1>
                                <p class="lead">{ex.syncingLead}</p>
                                {@render bar(
                                    externalPercent,
                                    externalConnection
                                        && externalOnboardingAction(externalConnection) === 'sync'
                                        ? ex.syncTitle
                                        : ex.restoring,
                                )}
                                {#if externalJob}
                                    <p class="meta">
                                        <span
                                            >{externalPercent === null
                                                ? strings.risuNest.importDialog.preparing
                                                : `${externalPercent}%`}</span
                                        >
                                        <span class="dim"
                                            >{externalStrings.jobActive[externalJob.kind]}</span
                                        >
                                    </p>
                                {/if}
                            {:else if externalStage === 'conflict'}
                                <h1>{ex.conflictTitle}</h1>
                                <p class="lead">{ex.conflictLead}</p>
                                {@render externalRepositoryChip()}
                                <div class="actions">
                                    <button
                                        class="btn primary"
                                        type="button"
                                        disabled={externalWorking}
                                        onclick={() => void takeExternalRepository()}
                                    >
                                        {externalConflict
                                            && externalOnboardingConflictStep(externalConflict)
                                                === 'receive-repository'
                                            ? ex.conflictReceive
                                            : ex.conflictTake}
                                    </button>
                                    <button
                                        class="btn ghost"
                                        type="button"
                                        disabled={externalWorking}
                                        onclick={() => {
                                            resetExternal()
                                            goTo('home')
                                        }}
                                    >
                                        {ex.other}
                                    </button>
                                </div>
                            {:else if externalStage === 'error'}
                                <h1>{ex.title}</h1>
                                {@render externalRepositoryChip()}
                                <p class="result failed" role="status">{ex.errorSummary}</p>
                                <p class="reason">{externalError || ex.errorReason}</p>
                                <div class="actions">
                                    <button
                                        class="btn primary"
                                        type="button"
                                        disabled={externalWorking}
                                        onclick={() => {
                                            externalError = ''
                                            externalStage = externalConnection ? 'choose' : 'connect'
                                            if (
                                                externalConnection
                                                && externalOnboardingAction(externalConnection)
                                                    === 'restore'
                                                && externalHistory.length === 0
                                            ) {
                                                void loadExternalHistory(false)
                                            }
                                        }}
                                    >
                                        {ex.retry}
                                    </button>
                                    <button
                                        class="btn ghost"
                                        type="button"
                                        onclick={() => {
                                            resetExternal()
                                            goTo('home')
                                        }}
                                    >
                                        {t.backHome}
                                    </button>
                                </div>
                            {:else if externalStage === 'choose' && externalConnection}
                                {@const connection = externalConnection}
                                <h1>
                                    {externalOnboardingAction(connection) === 'sync'
                                        ? ex.syncTitle
                                        : ex.restoreTitle}
                                </h1>
                                {@render externalRepositoryChip()}
                                {#if externalOnboardingAction(connection) === 'sync'}
                                    <p class="lead">{ex.syncDesc}</p>
                                    <div class="actions">
                                        <button
                                            class="btn primary big"
                                            type="button"
                                            disabled={externalWorking}
                                            onclick={() => void startExternalSync()}
                                        >
                                            {ex.syncStart}
                                        </button>
                                        <button
                                            class="btn ghost"
                                            type="button"
                                            disabled={externalWorking}
                                            onclick={() => {
                                                resetExternal()
                                                goTo('home')
                                            }}
                                        >
                                            {ex.other}
                                        </button>
                                    </div>
                                {:else if externalRestorable.length === 0}
                                    <p class="lead">{ex.restoreEmpty}</p>
                                    <div class="actions">
                                        <button class="btn primary" type="button" onclick={startFresh}>
                                            {t.home.freshTitle}
                                        </button>
                                    </div>
                                {:else}
                                    <p class="lead">{ex.restoreLead}</p>
                                    <div class="picks" role="radiogroup" aria-label={ex.restoreTitle}>
                                        {#each externalRestorable as item (item.id)}
                                            <label class="pick" class:on={externalSelected === item.id}>
                                                <input
                                                    type="radio"
                                                    name="external-backup"
                                                    value={item.id}
                                                    bind:group={externalSelected}
                                                />
                                                <span class="when">{externalWhen(item.createdAtMs)}</span>
                                                <span class="dim">{externalSize(item.storedBytes)}</span>
                                            </label>
                                        {/each}
                                    </div>
                                    {#if externalCursor}
                                        <div class="actions">
                                            <button
                                                class="btn ghost"
                                                type="button"
                                                disabled={externalWorking}
                                                onclick={() => void loadExternalHistory(true)}
                                            >
                                                {ex.restoreMore}
                                            </button>
                                        </div>
                                    {/if}
                                    <p class="note warn">
                                        <TriangleAlert /><span
                                            >{externalOnboardingRestoreRestarts()
                                                ? ex.restoreRestartNote
                                                : ex.restoreNote}</span
                                        >
                                    </p>
                                    <div class="actions">
                                        <button
                                            class="btn primary"
                                            type="button"
                                            disabled={externalWorking || !externalSelected}
                                            onclick={() => void restoreExternalBackup()}
                                        >
                                            {ex.restore}
                                        </button>
                                        <button
                                            class="btn ghost"
                                            type="button"
                                            disabled={externalWorking}
                                            onclick={() => {
                                                resetExternal()
                                                goTo('home')
                                            }}
                                        >
                                            {ex.other}
                                        </button>
                                    </div>
                                {/if}
                                {#if externalError}
                                    <p class="reason" role="alert">{externalError}</p>
                                {/if}
                            {:else}
                                {@render back('sync', t.back)}
                                <h1>{ex.title}</h1>
                                <p class="lead">{ex.lead}</p>
                                <ol class="howto">
                                    <li>{ex.stepKey}</li>
                                    <li>{ex.stepCode}</li>
                                    <li>{ex.stepPick}</li>
                                </ol>
                                {#key externalKey}
                                    <ConnectionForm
                                        strings={externalStrings}
                                        restoreOnly
                                        tone="onboarding"
                                        onconnected={onExternalConnected}
                                        oncancel={() => goTo('sync')}
                                    />
                                {/key}
                            {/if}
                        {:else if flow.state === 'sync-account'}
                            {@render back('sync', t.back)}
                            <h1>{t.account.title}</h1>
                            <p class="lead">{t.account.lead}</p>
                            <div class="actions">
                                {#if DBState.db.account}
                                    <button
                                        class="btn primary big"
                                        type="button"
                                        onclick={() =>
                                            goTo('sync-account-found')}
                                    >
                                        <User />{t.account.cont}
                                    </button>
                                {:else}
                                    <button
                                        class="btn primary big"
                                        type="button"
                                        onclick={openAccountLogin}
                                    >
                                        <User />{t.account.login}
                                    </button>
                                {/if}
                            </div>
                            <p class="hint spaced">
                                <Info /><span>{t.account.hint}</span>
                            </p>
                        {:else if flow.state === 'sync-account-found'}
                            {@render back('sync', t.back)}
                            <h1>{t.accountFound.title}</h1>
                            <div class="account">
                                <span class="av"><User /></span>
                                <span
                                    >{t.account.signedIn.replace(
                                        '{0}',
                                        DBState.db.account?.id ?? '',
                                    )}</span
                                >
                            </div>
                            <div class="found">
                                <span class="ic"><Cloud /></span>
                                <div>
                                    <b>{t.accountFound.cardTitle}</b><small
                                        >{t.accountFound.cardDesc}</small
                                    >
                                </div>
                            </div>
                            <p class="note">
                                <Info /><span>{t.accountFound.note}</span>
                            </p>
                            <div class="actions">
                                <button
                                    class="btn primary"
                                    type="button"
                                    disabled={accountBusy}
                                    onclick={restoreAccountBackup}
                                >
                                    {t.accountFound.restore}
                                </button>
                                <button
                                    class="btn ghost"
                                    type="button"
                                    disabled={accountBusy}
                                    onclick={() => goTo('home')}
                                >
                                    {t.accountFound.other}
                                </button>
                            </div>
                        {:else}
                            <div class="done">
                                <span class="check-ring"><Check /></span>
                                <h1>{t.done.title}</h1>
                                <p class="lead flush">
                                    {t.done[onboardingSummary(flow.path)]}
                                </p>
                                <button
                                    class="btn primary big"
                                    type="button"
                                    onclick={finish}
                                >
                                    {t.done.start}<ArrowRight />
                                </button>
                            </div>
                        {/if}
                    </div>
                {/key}
            {/if}
        </section>
    </div>
</div>

{#if loginOpen}
    <div class="login-scrim" role="dialog" aria-modal="true" aria-label={t.account.login}>
        <div class="login-bar">
            <span class="login-title">{t.account.login}</span>
            <button
                class="login-close"
                type="button"
                title={t.account.closeLogin}
                aria-label={t.account.closeLogin}
                onclick={closeAccountLogin}
            >
                <XIcon />
            </button>
        </div>
        <iframe bind:this={loginFrame} src={loginUrl} title={t.account.login}
        ></iframe>
    </div>
{/if}

<style>
    /* A container query never styles its own container, so the query context is
       one level above the grid it resizes. */
    .onb-root {
        container-type: inline-size;
        width: 100%;
        height: 100%;
    }
    /* Korean has no spaces inside a word, so the default break rule splits words
       mid-syllable. Chinese keeps the default, where breaking anywhere is right. */
    .onb-root.keep-all {
        word-break: keep-all;
    }
    .onb {
        --o-ink: var(--color-textcolor);
        --o-soft: color-mix(in srgb, var(--color-textcolor) 64%, transparent);
        --o-faint: color-mix(in srgb, var(--color-textcolor) 40%, transparent);
        --o-line: var(--color-darkborderc);
        --o-hover: var(--color-selected);
        --o-panel: var(--color-darkbg);
        --o-btn: var(--color-darkbutton);
        --o-blue: var(--color-primary-500);
        --o-ok: var(--color-success-500);
        --o-warn: var(--color-danger-400);
        /* The logo gradient. Onboarding-only, not part of the theme. */
        --o-teal: #22c8c6;
        --o-indigo: #6e7cf8;
        --o-safe-top: env(safe-area-inset-top, 0px);

        display: grid;
        grid-template-rows: 262px 1fr;
        width: 100%;
        height: 100%;
        overflow: hidden;
        background: var(--color-bgcolor);
        color: var(--o-ink);
        font-size: 14px;
        line-height: 1.5;
    }
    .onb :is(button, select) {
        font: inherit;
        color: inherit;
        cursor: pointer;
    }
    .onb button:disabled {
        opacity: 0.55;
        cursor: default;
    }
    .onb :is(button, select):focus-visible {
        outline: 2px solid var(--o-blue);
        outline-offset: 2px;
    }
    .sr-only {
        position: absolute;
        width: 1px;
        height: 1px;
        overflow: hidden;
        clip-path: inset(50%);
        white-space: nowrap;
    }

    /* ── brand panel ── */
    .brand {
        position: relative;
        overflow: hidden;
        padding: calc(18px + var(--o-safe-top)) 22px 30px;
        display: flex;
        flex-direction: column;
        justify-content: space-between;
    }
    .brand canvas {
        position: absolute;
        inset: 0;
        width: 100%;
        height: 100%;
    }
    .brand > *:not(canvas) {
        position: relative;
    }
    .brand-top {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
    }
    .brand .wm {
        width: 132px;
        height: auto;
    }
    .brand .eyebrow {
        margin: 0 0 8px;
        font-size: 11px;
        font-weight: 600;
        letter-spacing: 0.14em;
        color: var(--o-teal);
    }
    .brand h2 {
        margin: 0;
        font-size: 22px;
        font-weight: 700;
        line-height: 1.28;
        letter-spacing: -0.01em;
        text-wrap: balance;
    }
    /* No width cap: `ch` is narrow next to CJK glyphs, so a cap broke every
       description in two well before the panel edge. */
    .brand .desc {
        display: none;
        margin: 10px 0 0;
        font-size: 13.5px;
        color: var(--o-soft);
    }

    /* ── step indicator ── */
    .steps {
        display: flex;
        gap: 6px;
        margin: 0;
        padding: 0;
        list-style: none;
    }
    .steps.bottom {
        display: none;
    }
    .steps li {
        display: flex;
        align-items: center;
        gap: 8px;
        font-size: 12.5px;
        color: var(--o-faint);
    }
    .steps li i {
        display: block;
        width: 7px;
        height: 7px;
        border-radius: 50%;
        background: color-mix(in srgb, var(--o-ink) 22%, transparent);
    }
    .steps li span {
        display: none;
    }
    .steps li.on {
        color: var(--o-ink);
    }
    .steps li.on i {
        background: var(--o-teal);
        box-shadow: 0 0 0 3px rgba(34, 200, 198, 0.22);
    }
    .steps li.past i {
        background: rgba(34, 200, 198, 0.55);
    }

    /* ── content panel ── */
    .panel {
        position: relative;
        min-height: 0;
        margin-top: -18px;
        padding: 24px 20px 22px;
        border-radius: 22px 22px 0 0;
        background: var(--o-panel);
        overflow-y: auto;
    }
    .panel-in {
        display: flex;
        flex-direction: column;
        min-height: 100%;
        animation: onboarding-rise 0.35s ease-out;
    }
    @keyframes onboarding-rise {
        from {
            opacity: 0;
            transform: translateY(8px);
        }
        to {
            opacity: 1;
            transform: none;
        }
    }
    .panel h1 {
        margin: 0 0 6px;
        font-size: 22px;
        font-weight: 700;
        line-height: 1.28;
        letter-spacing: -0.01em;
        text-wrap: balance;
    }
    .lead {
        margin: 0 0 20px;
        font-size: 13.5px;
        color: var(--o-soft);
    }
    .lead.flush {
        margin: 0;
    }
    .back {
        display: inline-flex;
        align-self: flex-start;
        align-items: center;
        gap: 3px;
        margin-bottom: 14px;
        font-size: 13px;
        color: var(--o-soft);
    }
    .back :global(svg) {
        width: 16px;
        height: 16px;
    }
    .panel-foot {
        display: flex;
        justify-content: space-between;
        align-items: center;
        gap: 12px;
        margin-top: auto;
        padding-top: 22px;
        font-size: 12px;
        color: var(--o-faint);
    }

    /* ── option rows ── */
    .rows {
        display: flex;
        flex-direction: column;
        gap: 10px;
    }
    .row {
        display: grid;
        grid-template-columns: 40px 1fr 16px;
        align-items: center;
        gap: 14px;
        padding: 13px 14px;
        border: 1px solid var(--o-line);
        border-radius: 14px;
        background: color-mix(in srgb, var(--o-ink) 2%, transparent);
        text-align: start;
        transition:
            background 0.15s,
            border-color 0.15s;
    }
    .row:hover {
        background: var(--o-hover);
        border-color: var(--color-borderc);
    }
    .row .ic {
        display: grid;
        place-items: center;
        width: 40px;
        height: 40px;
        border-radius: 11px;
        background: color-mix(in srgb, var(--o-ink) 6%, transparent);
    }
    .row b {
        display: block;
        font-size: 15px;
        font-weight: 600;
        line-height: 1.3;
    }
    .row small {
        display: block;
        margin-top: 3px;
        font-size: 12.5px;
        line-height: 1.45;
        color: var(--o-soft);
    }
    .row .chev {
        display: grid;
        color: var(--o-faint);
    }
    .row .chev :global(svg) {
        width: 16px;
        height: 16px;
    }
    .row.primary {
        border-color: transparent;
        background:
            linear-gradient(var(--o-panel), var(--o-panel)) padding-box,
            linear-gradient(
                    120deg,
                    var(--o-teal),
                    var(--o-blue),
                    var(--o-indigo)
                )
                border-box;
    }
    .row.primary:hover {
        background:
            linear-gradient(
                    color-mix(in srgb, var(--o-panel) 82%, var(--o-hover)),
                    color-mix(in srgb, var(--o-panel) 82%, var(--o-hover))
                )
                padding-box,
            linear-gradient(
                    120deg,
                    var(--o-teal),
                    var(--o-blue),
                    var(--o-indigo)
                )
                border-box;
    }
    .row.primary .ic {
        background: linear-gradient(135deg, var(--o-teal), var(--o-indigo));
        color: #fff;
    }
    .row .ic :global(svg),
    .ic :global(svg) {
        width: 20px;
        height: 20px;
    }

    /* ── controls ── */
    .pill {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        padding: 6px 11px;
        border: 1px solid var(--o-line);
        border-radius: 999px;
        font-size: 12.5px;
        color: var(--o-soft);
    }
    .pill :global(svg) {
        width: 14px;
        height: 14px;
    }
    .pill select {
        border: 0;
        background: none;
        font-size: 12.5px;
        outline: none;
    }
    .btn {
        display: inline-flex;
        align-items: center;
        justify-content: center;
        gap: 8px;
        padding: 10px 16px;
        border: 1px solid var(--o-line);
        border-radius: 10px;
        background: var(--o-btn);
        font-size: 13.5px;
        font-weight: 600;
        transition: background 0.15s;
    }
    .btn:hover:not(:disabled) {
        background: var(--o-hover);
    }
    .btn :global(svg) {
        width: 16px;
        height: 16px;
    }
    .btn.primary {
        border-color: transparent;
        background: var(--o-blue);
        color: #fff;
    }
    .btn.primary:hover:not(:disabled) {
        background: var(--color-primary-600);
    }
    .btn.ghost {
        background: transparent;
    }
    .btn.big {
        padding: 13px 22px;
        font-size: 15px;
    }
    .actions {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 10px;
    }

    /* ── import ── */
    .drop {
        display: flex;
        flex-direction: column;
        align-items: center;
        gap: 6px;
        padding: 26px 18px;
        border: 1px solid var(--o-line);
        border-radius: 16px;
        background: rgba(59, 130, 246, 0.05);
        text-align: center;
    }
    .drop .ic {
        display: grid;
        place-items: center;
        width: 44px;
        height: 44px;
        margin-bottom: 4px;
        border-radius: 12px;
        background: color-mix(in srgb, var(--o-ink) 6%, transparent);
    }
    .drop b {
        font-size: 14.5px;
        font-weight: 600;
    }
    .drop .btn {
        margin-top: 4px;
    }
    .hint {
        display: flex;
        align-items: flex-start;
        gap: 8px;
        margin: 12px 0 0;
        font-size: 12.5px;
        color: var(--o-soft);
    }
    .hint.spaced {
        margin-top: 16px;
    }
    .hint :global(svg) {
        flex: none;
        width: 15px;
        height: 15px;
        margin-top: 2px;
    }
    .detect {
        display: grid;
        grid-template-columns: 36px 1fr;
        align-items: center;
        gap: 12px;
        margin-top: 16px;
        padding: 12px 14px;
        border: 1px solid rgba(34, 200, 198, 0.35);
        border-radius: 14px;
        background: rgba(34, 200, 198, 0.08);
    }
    .detect .ic {
        display: grid;
        place-items: center;
        width: 36px;
        height: 36px;
        border-radius: 10px;
        background: rgba(34, 200, 198, 0.18);
        color: var(--o-teal);
    }
    .detect .ic :global(svg) {
        width: 18px;
        height: 18px;
    }
    .detect b {
        display: block;
        font-size: 13.5px;
        font-weight: 600;
    }
    .detect small {
        display: block;
        font-size: 12px;
        color: var(--o-soft);
    }

    /* ── connect ── */
    .howto {
        display: flex;
        flex-direction: column;
        gap: 10px;
        margin: 0 0 18px;
        padding: 0;
        list-style: none;
        counter-reset: onboarding-step;
    }
    .howto li {
        display: grid;
        grid-template-columns: 24px 1fr;
        align-items: start;
        gap: 10px;
        font-size: 13.5px;
        color: var(--o-soft);
    }
    .howto li::before {
        counter-increment: onboarding-step;
        content: counter(onboarding-step);
        display: grid;
        place-items: center;
        width: 24px;
        height: 24px;
        border-radius: 50%;
        background: color-mix(in srgb, var(--o-ink) 8%, transparent);
        font-size: 12px;
        font-weight: 600;
        color: var(--o-ink);
    }
    .connect {
        display: flex;
        flex-direction: column;
        gap: 12px;
    }
    .field {
        display: flex;
        align-items: center;
        gap: 8px;
        padding: 5px 5px 5px 12px;
        border: 1px solid var(--o-line);
        border-radius: 12px;
        background: color-mix(in srgb, var(--o-ink) 3%, transparent);
    }
    .field :global(svg) {
        flex: none;
        width: 16px;
        height: 16px;
        color: var(--o-faint);
    }
    .error {
        margin: 0;
        font-size: 12.5px;
        color: var(--color-draculared);
    }

    /* ── progress: native file imports ── */
    .file {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 6px 10px;
        padding: 10px 12px;
        border: 1px solid var(--o-line);
        border-radius: 12px;
        font-size: 13.5px;
    }
    .file > :global(svg) {
        flex: none;
        width: 18px;
        height: 18px;
        color: var(--o-soft);
    }
    .file .name {
        min-width: 0;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        font-weight: 600;
    }
    .file .tag {
        padding: 2px 8px;
        border-radius: 999px;
        background: rgba(34, 200, 198, 0.14);
        font-size: 12px;
        color: var(--o-teal);
    }
    .dim {
        font-size: 12.5px;
        color: var(--o-faint);
    }
    .track {
        width: 100%;
        height: 8px;
        margin: 18px 0 8px;
        border-radius: 999px;
        background: color-mix(in srgb, var(--o-ink) 8%, transparent);
        overflow: hidden;
    }
    .fill {
        height: 100%;
        border-radius: 999px;
        background: linear-gradient(90deg, var(--o-teal), var(--o-blue));
        transition: width 0.3s;
    }
    .fill.pulse {
        animation: onboarding-pulse 1.4s ease-in-out infinite;
    }
    @keyframes onboarding-pulse {
        0%,
        100% {
            opacity: 0.55;
        }
        50% {
            opacity: 0.2;
        }
    }
    .meta {
        display: flex;
        flex-wrap: wrap;
        gap: 4px 12px;
        margin: 0 0 16px;
        font-size: 13px;
        font-variant-numeric: tabular-nums;
        color: var(--o-soft);
    }
    .meta > :last-child {
        margin-inline-start: auto;
    }
    .result {
        margin: 0 0 6px;
        font-size: 14px;
        font-weight: 600;
    }
    .result.succeeded {
        color: var(--o-ok);
    }
    .result.failed {
        color: var(--color-draculared);
    }
    .result.cancelled {
        color: var(--o-soft);
    }
    .reason {
        margin: 0 0 14px;
        font-size: 13px;
        color: var(--o-soft);
    }
    .stages {
        display: flex;
        flex-direction: column;
        gap: 6px;
        margin: 0 0 16px;
        padding: 0;
        list-style: none;
        font-size: 13.5px;
    }
    .stages li {
        display: grid;
        grid-template-columns: 18px 1fr auto;
        align-items: center;
        gap: 10px;
    }
    .stages li[data-state='pending'] {
        color: var(--o-faint);
    }
    .stages .mark {
        display: grid;
        place-items: center;
        width: 18px;
        height: 18px;
        color: var(--o-teal);
    }
    .stages li[data-state='pending'] .mark::before {
        content: '';
        width: 12px;
        height: 12px;
        border: 1px solid var(--o-line);
        border-radius: 50%;
    }
    .stages li[data-state='stopped'] .mark {
        color: var(--color-draculared);
    }
    .stages .mark :global(svg) {
        width: 16px;
        height: 16px;
    }
    .stages li[data-state='active'] .mark :global(svg) {
        animation: onboarding-spin 1s linear infinite;
    }
    @keyframes onboarding-spin {
        to {
            transform: rotate(360deg);
        }
    }
    .stages .label {
        min-width: 0;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
    }
    .stages .detail {
        font-size: 12.5px;
        font-variant-numeric: tabular-nums;
        color: var(--o-soft);
    }
    .item {
        margin: -8px 0 16px;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        font-family: ui-monospace, Consolas, monospace;
        font-size: 12px;
        color: var(--o-faint);
    }
    /* As many columns as fit, so a wide panel lays every card out in one row
       and a phone keeps three or four per row. The row stops growing at 160px
       per card, which keeps a two-card set card-sized. */
    .counts {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(84px, 1fr));
        gap: 8px;
        max-width: calc(var(--cards) * 160px + (var(--cards) - 1) * 8px);
        margin: 0 0 16px;
    }
    .counts div {
        min-width: 0;
        padding: 8px;
        border: 1px solid var(--o-line);
        border-radius: 10px;
        overflow-wrap: anywhere;
    }
    .counts dt {
        font-size: 12px;
        color: var(--o-soft);
    }
    .counts dd {
        margin: 2px 0 0;
        font-size: 14px;
        font-weight: 600;
        font-variant-numeric: tabular-nums;
    }
    .warnings {
        margin: 0 0 16px;
        padding: 10px 12px 10px 28px;
        border: 1px solid color-mix(in srgb, var(--o-warn) 35%, transparent);
        border-radius: 10px;
        font-size: 12.5px;
        color: var(--o-warn);
    }
    .details {
        display: flex;
        flex-direction: column;
        align-items: flex-start;
        gap: 8px;
        margin: 0 0 16px;
    }
    .details-body {
        position: relative;
        width: 100%;
    }
    .details-body pre {
        max-height: 180px;
        margin: 0;
        padding: 10px 40px 10px 12px;
        overflow: auto;
        border: 1px solid var(--o-line);
        border-radius: 10px;
        background: color-mix(in srgb, var(--o-ink) 3%, transparent);
        font-size: 12px;
        white-space: pre-wrap;
        word-break: break-all;
        color: var(--o-soft);
    }
    .details-body .copy {
        position: absolute;
        top: 6px;
        right: 6px;
        display: grid;
        place-items: center;
        width: 26px;
        height: 26px;
        border: 0;
        border-radius: 6px;
        background: none;
        color: var(--o-soft);
    }
    .details-body .copy :global(svg) {
        width: 14px;
        height: 14px;
    }

    /* ── account ── */
    .account {
        display: flex;
        align-items: center;
        gap: 10px;
        margin-bottom: 14px;
        font-size: 13px;
        color: var(--o-soft);
    }
    .account .av {
        display: grid;
        place-items: center;
        width: 28px;
        height: 28px;
        border-radius: 50%;
        background: linear-gradient(135deg, var(--o-teal), var(--o-indigo));
        color: #fff;
    }
    .account .av :global(svg) {
        width: 15px;
        height: 15px;
    }
    /* ── external storage: the backups a repository holds ── */
    .picks {
        display: grid;
        gap: 8px;
        margin: 14px 0 4px;
        max-height: 244px;
        overflow-y: auto;
    }
    .pick {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 4px 10px;
        padding: 10px 12px;
        border: 1px solid var(--o-line);
        border-radius: 12px;
        font-size: 13.5px;
        cursor: pointer;
    }
    .pick.on {
        border-color: rgba(34, 200, 198, 0.55);
        background: rgba(34, 200, 198, 0.08);
    }
    .pick input {
        flex: none;
        accent-color: var(--o-teal);
    }
    .pick .when {
        min-width: 0;
        flex: 1 1 auto;
        font-variant-numeric: tabular-nums;
    }
    .found {
        display: grid;
        grid-template-columns: 40px 1fr;
        align-items: center;
        gap: 12px;
        margin-bottom: 14px;
        padding: 14px;
        border: 1px solid rgba(34, 200, 198, 0.35);
        border-radius: 14px;
        background: rgba(34, 200, 198, 0.08);
    }
    .found .ic {
        display: grid;
        place-items: center;
        width: 40px;
        height: 40px;
        border-radius: 11px;
        background: rgba(34, 200, 198, 0.18);
        color: var(--o-teal);
    }
    .found b {
        display: block;
        font-size: 14.5px;
        font-weight: 600;
    }
    .found small {
        display: block;
        font-size: 12.5px;
        color: var(--o-soft);
    }
    .note {
        display: flex;
        gap: 8px;
        margin: 0 0 16px;
        padding: 10px 12px;
        border-radius: 10px;
        background: color-mix(in srgb, var(--o-ink) 4%, transparent);
        font-size: 12.5px;
        line-height: 1.5;
        color: var(--o-soft);
    }
    .note :global(svg) {
        flex: none;
        width: 15px;
        height: 15px;
        margin-top: 2px;
    }
    .note.warn {
        margin-top: 16px;
        background: color-mix(in srgb, var(--o-warn) 8%, transparent);
        color: var(--o-warn);
    }

    /* ── done ── */
    .done {
        display: flex;
        flex-direction: column;
        align-items: flex-start;
        gap: 4px;
        margin: auto 0;
    }
    .check-ring {
        display: grid;
        place-items: center;
        width: 58px;
        height: 58px;
        margin-bottom: 14px;
        border-radius: 50%;
        background: linear-gradient(135deg, var(--o-teal), var(--o-indigo));
        color: #fff;
        box-shadow: 0 12px 30px rgba(59, 130, 246, 0.35);
    }
    .check-ring :global(svg) {
        width: 28px;
        height: 28px;
    }
    .done .btn {
        margin-top: 8px;
    }

    /* ── account login frame ── */
    .login-scrim {
        position: fixed;
        inset: 0;
        z-index: 50;
        display: flex;
        flex-direction: column;
        padding: calc(12px + var(--o-safe-top)) 12px
            calc(12px + env(safe-area-inset-bottom, 0px));
        background: rgba(0, 0, 0, 0.55);
        color: var(--color-textcolor);
    }
    .login-bar {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 10px;
        padding: 8px 8px 8px 14px;
        border: 1px solid var(--color-darkborderc);
        border-bottom: 0;
        border-radius: 12px 12px 0 0;
        background: var(--color-darkbg);
    }
    .login-title {
        font-size: 13.5px;
        font-weight: 600;
    }
    .login-close {
        display: grid;
        place-items: center;
        width: 32px;
        height: 32px;
        border: 1px solid var(--color-darkborderc);
        border-radius: 8px;
        background: var(--color-darkbutton);
        color: inherit;
        cursor: pointer;
    }
    .login-close:hover {
        background: var(--color-selected);
    }
    .login-close:focus-visible {
        outline: 2px solid var(--color-primary-500);
        outline-offset: 2px;
    }
    .login-close :global(svg) {
        width: 16px;
        height: 16px;
    }
    .login-scrim iframe {
        flex: 1;
        width: 100%;
        border: 0;
        border-radius: 0 0 12px 12px;
        background: #fff;
    }

    @container (min-width: 720px) {
        .onb {
            grid-template-rows: none;
            grid-template-columns: 42% 1fr;
        }
        .brand {
            padding: 34px 36px 30px;
        }
        .brand .wm {
            width: 150px;
        }
        .brand h2 {
            font-size: 27px;
        }
        .brand .desc {
            display: block;
        }
        .brand-top .steps {
            display: none;
        }
        .steps.bottom {
            display: flex;
            gap: 18px;
        }
        .steps li span {
            display: inline;
        }
        .panel {
            margin: 0;
            padding: 44px 48px 36px;
            border-radius: 0;
            display: flex;
            flex-direction: column;
        }
        .panel-in {
            min-height: 0;
            margin: auto 0;
        }
        .panel h1 {
            font-size: 25px;
        }
        .panel-foot {
            margin-top: 28px;
        }
    }

    @media (prefers-reduced-motion: reduce) {
        .panel-in,
        .fill.pulse,
        .stages li[data-state='active'] .mark :global(svg) {
            animation: none;
        }
    }
</style>
