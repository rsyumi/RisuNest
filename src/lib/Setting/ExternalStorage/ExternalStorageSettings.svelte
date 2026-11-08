<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import ConnectionSettingsDialog from './ConnectionSettingsDialog.svelte'
    import { runWithMobileBackgroundTask, measuredTaskPercent } from 'src/ts/mobileBackgroundTask'
    import TextInput from 'src/lib/UI/GUI/TextInput.svelte'
    import NumberInput from 'src/lib/UI/GUI/NumberInput.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import ExternalTransferProgress from './ExternalTransferProgress.svelte'
    import { beginExternalProgress, clearExternalProgress, externalProgressFailure, externalJobFraction, externalJobTotalGrows, externalProgressFor, subscribeExternalProgress, type ExternalOperationProgress } from 'src/ts/storage/sync/external/progress'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SettingNotice from '../RisuNest/SettingNotice.svelte'
    import StatusBadge from '../RisuNest/StatusBadge.svelte'
    import { CloudIcon, DatabaseIcon, MonitorSmartphoneIcon, PackageIcon, PinIcon, ServerIcon } from '@lucide/svelte'
    import { alertActionConfirm, alertConfirm, alertNormal, alertCheckboxConfirm } from 'src/ts/alert'
    import { DBState } from 'src/ts/stores.svelte'
    import { getExternalStorageBridge } from 'src/ts/storage/sync/external/bridge'
    import { bindSyncTarget, prepareBoundLibraryReplacement, resumeCurrentSyncBinding, unbindSyncTarget } from 'src/ts/storage/sync/bindingRegistry'
    import { hasLibraryCharacters } from 'src/ts/storage/libraryCharacters'
    import { createNativeSyncBindingBridge } from 'src/ts/storage/sync/bindingNative'
    import { subscribeSyncBindingChanges } from 'src/ts/storage/sync/bindingChanges'
    import { isExternalLwwRunning, requestExternalLwwNow, subscribeExternalLwwCatchUp, subscribeExternalLwwFailures, supportsExternalLwwNewDevice, switchExternalLwwLibraryFormat } from 'src/ts/storage/sync/external/lwwProduction'
    import { externalErrorLabel, externalFormatCode, externalSyncFailureText, externalSyncStatusView } from 'src/ts/storage/sync/syncStatusView'
    import BindingTargetSwitch from 'src/ts/storage/sync/BindingTargetSwitch.svelte'
    import { PREVIOUS_FILES_DOWNLOAD_FAILED } from 'src/ts/storage/sync/bindingFlow'
    import { language } from 'src/lang'
    import { externalJobIsActive, externalJobIsPaused, externalJobProgress, mergeExternalHistoryItems } from 'src/ts/storage/sync/external/connection'
    import {
        refreshExternalStorageProductionState,
        requestExternalStorageNow,
        resumeExternalStorageJob,
        requestExternalStorageRestore,
        requestExternalStorageDeleteHistory,
        startExternalStorageMaintenanceJob,
        stopExternalStorageRestore,
    } from 'src/ts/storage/sync/external/production'
    import type {
        ExternalConnectionResult,
        ExternalConnectionSummary,
        ExternalHistoryItem,
        ExternalJobSummary,
        ExternalQuotaSummary,
        ExternalConnectionSettingsMaterial,
        ExternalRetentionPolicy,
        ExternalRestoreArea,
        ExternalStorageState,
        ExternalSnapshotExportProgress,
    } from 'src/ts/storage/sync/external/types'
    import { externalRestoreAreas } from 'src/ts/storage/sync/external/restoreScope'
    import { countConnectionOnlyAssets, downloadRemoteAssets } from 'src/ts/storage/sync/serverAssetResidency'
    import { formatRisuNestStorageBytes } from 'src/ts/storage/risuNestStorageDashboard'
    import { createRemainingTimeEstimator } from 'src/ts/storage/sync/remainingTime'
    import ConnectionForm from './ConnectionForm.svelte'
    import { externalConnectionPlace, externalConnectionTitle, externalErrorKind, externalErrorMessage, externalStorageStrings } from './strings'

    const bridge = getExternalStorageBridge()
    /** How many empty history pages one request reads past before stopping. */
    const EMPTY_HISTORY_STEPS = 4
    /** What a repository accepts for each retention limit. */
    const RETENTION_LIMITS = { keepCount: [1, 1000], keepDays: [7, 3650] } as const
    const strings = $derived(externalStorageStrings(DBState.db.language))
    let storageState = $state<ExternalStorageState | null>(null)
    let adding = $state(false)
    let addressConnection = $state<ExternalConnectionSummary | undefined>()
    let renewalConnection = $state<ExternalConnectionSummary | undefined>()
    let unlockConnection = $state<ExternalConnectionSummary | undefined>()
    let unlockKey = $state('')
    let destroyed = false
    let stopJobEvents: (() => void) | undefined
    let stopSyncFailures: (() => void) | undefined
    let stopSyncCatchUp: (() => void) | undefined
    let stopProgress: (() => void) | undefined
    let progress = $state<ReadonlyMap<string, ExternalOperationProgress>>(new Map())
    let syncFailures = $state<ReadonlyMap<string, unknown>>(new Map())
    let syncCatchingUp = $state<ReadonlySet<string>>(new Set())
    let busy = $state(false)
    /** Which action is running, so only the button that started it shows a spinner. */
    let activeAction = $state('')
    let error = $state('')
    /** A failed background refresh, which the next successful refresh clears. */
    let pollError = $state('')
    /** The details tab picked on each card. A card shows its history until another tab is picked. */
    let expanded = $state<Record<string, 'history' | 'quota'>>({})
    let history = $state<Record<string, ExternalHistoryItem[]>>({})
    let historyError = $state<Record<string, string>>({})
    /** Cards whose history was read once, so a failing remote is not read again on every poll. */
    const historyRequested = new Set<string>()
    let historyCursor = $state<Record<string, string | undefined>>({})
    let historyLoading = $state<Record<string, boolean>>({})
    let quota = $state<Record<string, ExternalQuotaSummary>>({})
    const quotaLoading = new Map<string, Promise<void>>()
    const quotaRequested = new Set<string>()
    let remoteOnly = $state<Record<string, number | null>>({})
    let removalDownload = $state<{ connectionId: string; controller: AbortController; cancelling: boolean } | null>(null)
    /** The disconnect waiting for its file check, which cancelling abandons. */
    let removalCheck = $state<{ connectionId: string; controller: AbortController } | null>(null)
    let recoveryKey = $state('')
    let recoveryPanel = $state<HTMLDivElement | undefined>()
    let connectionSettings = $state<ExternalConnectionSettingsMaterial | null>(null)
    /** The history entry whose restore scope is open, and what is ticked in it. */
    let exportRun = $state<{ id: string; connectionId: string; progress?: ExternalSnapshotExportProgress } | null>(null)
    /** Jobs that add or change history rows, by job ID, so the open history reloads when they finish. */
    const historyJobs = new Map<string, string>()
    let pollTimer: ReturnType<typeof setTimeout> | undefined
    /** The state a sync switch was set to while its change runs. Otherwise it shows the device's sync target. */
    let syncRequest = $state<Record<string, boolean>>({})
    /** Whether sync with each connection was started in this session, read on every refresh. */
    let syncRunning = $state<Record<string, boolean>>({})
    /** A connection made for sync while its recovery key is on screen starts syncing once the key is closed. */
    let pendingSyncStart: ExternalConnectionSummary | undefined
    /** The state an automatic backup switch was set to while its change runs. Otherwise it shows the stored setting. */
    let automaticRequest = $state<Record<string, boolean>>({})
    let stopBindingChanges: (() => void) | undefined
    /** Time left of each connection's running transfer, once its rate is known. */
    let remaining = $state<Record<string, number>>({})
    const remainingEstimators = new Map<string, ReturnType<typeof createRemainingTimeEstimator>>()

    async function setSyncBinding(connection: ExternalConnectionSummary, enabled: boolean): Promise<void> {
        busy = true
        syncRequest[connection.id] = enabled
        try {
            const current = (await createNativeSyncBindingBridge().state()).target
            const selected = current.kind === 'external' && current.connectionId === connection.id
            // A start that left sync off keeps this target, so turning the switch on resumes it.
            if (enabled) await (selected ? resumeCurrentSyncBinding(current) : bindSyncTarget({ kind: 'external', connectionId: connection.id }))
            // A switch left over from an older state must not stop another sync target.
            else if (selected) await unbindSyncTarget()
            await refreshExternalStorageProductionState()
            error = ''
        } catch (reason) {
            error = externalErrorKind(reason) === PREVIOUS_FILES_DOWNLOAD_FAILED ? language.lwwSync.downloadFailedNotConnected : externalErrorMessage(strings, reason)
        }
        finally {
            await refresh(true)
            delete syncRequest[connection.id]
            busy = false
        }
    }
    async function syncNow(connection: ExternalConnectionSummary): Promise<void> {
        busy = true
        try { await requestExternalLwwNow(connection.id) }
        catch (reason) { error = externalErrorMessage(strings, reason) }
        finally {
            await refresh(true)
            busy = false
        }
    }
    async function switchFormat(connection: ExternalConnectionSummary): Promise<void> {
        const lww = language.lwwSync
        const choice = await alertCheckboxConfirm({ title: lww.formatSwitchTitle, description: lww.formatSwitchDescription, checkboxLabel: lww.formatSwitchAcknowledge, actionLabel: lww.formatSwitch, cancelLabel: strings.cancel, requireChecked: true })
        if (!choice.confirmed) return
        busy = true
        try {
            // A library already above this app's level stops sync with its own guidance, which the sync attempt records.
            try { await switchExternalLwwLibraryFormat(connection.id) } catch (reason) {
                if (externalFormatCode(reason) !== 'app-update-required') { error = lww.formatSwitchFailed; return }
            }
            await requestExternalLwwNow(connection.id)
        } catch (reason) { error = externalErrorMessage(strings, reason) }
        finally {
            await refresh(true)
            busy = false
        }
    }
    /** Makes a connection made for sync this device's sync target, asking first when another target is set. */
    async function startSync(connection: ExternalConnectionSummary): Promise<void> {
        const current = await createNativeSyncBindingBridge().state().then(binding => binding.target, reason => {
            error = externalErrorMessage(strings, reason)
            return undefined
        })
        if (!current) return
        if (current.kind !== 'none' && !await alertActionConfirm({
            title: strings.switchSyncTitle,
            description: current.kind === 'server' ? strings.switchSyncFromServer : strings.switchSyncFromExternal,
            actionLabel: strings.startSync,
            cancelLabel: strings.cancel,
        })) return
        await setSyncBinding(connection, true)
    }
    function beginRestore(connection: ExternalConnectionSummary, item: ExternalHistoryItem): void {
        try {
            void runJob(connection, 'restore', {
                snapshotId: item.snapshotId,
                restoreAreas: externalRestoreAreas(item, []),
            })
        } catch (reason) { error = externalErrorMessage(strings, reason) }
    }
    onMount(() => {
        const openExternalStorage = (event: Event) => {
            const providerId = (event as CustomEvent<{ providerId?: string }>).detail?.providerId
            if (providerId && providerId !== 'google_drive') return
            adding = true
            requestAnimationFrame(() => {
                document.getElementById('risunest-external-storage')?.scrollIntoView({ block: 'start' })
            })
        }
        window.addEventListener('risunest:open-external-storage', openExternalStorage)
        return () => window.removeEventListener('risunest:open-external-storage', openExternalStorage)
    })

    async function refresh(silent = false): Promise<void> {
        if (!silent) {
            busy = true
            activeAction = 'refresh'
        }
        try {
            storageState = await bridge.getState()
            syncRunning = Object.fromEntries(storageState.connections.map(connection => [connection.id, isExternalLwwRunning(connection.id)]))
            estimateRemaining(storageState)
            for (const connection of storageState.connections) {
                if (historyRequested.has(connection.id)) continue
                historyRequested.add(connection.id)
                void loadHistory(connection, false)
            }
            for (const job of storageState.jobs) {
                if (job.kind === 'backup' && externalJobIsActive(job)) historyJobs.set(job.id, job.connectionId)
            }
            for (const [jobId, connectionId] of historyJobs) {
                const completed = storageState.jobs.find(job => job.id === jobId)
                if (!completed || externalJobIsActive(completed)) continue
                historyJobs.delete(jobId)
                const connection = storageState.connections.find(item => item.id === connectionId)
                if (completed.state === 'succeeded' && connection && selectedTab(connectionId) === 'history') {
                    await loadHistory(connection, false)
                }
            }
            await Promise.all(storageState.connections.filter(connection => selectedTab(connection.id) === 'quota').map(loadQuota))
            if (!silent) error = ''
            pollError = ''
        } catch (reason) {
            if (silent) pollError = externalErrorMessage(strings, reason)
            else error = externalErrorMessage(strings, reason)
        } finally {
            if (!silent) {
                busy = false
                activeAction = ''
            }
        }
    }

    async function refreshWithHistory(): Promise<void> {
        await refresh()
        const open = storageState?.connections.filter(connection => selectedTab(connection.id) === 'history') ?? []
        await Promise.all(open.map(connection => loadHistory(connection, false)))
    }

    function estimateRemaining(state: ExternalStorageState): void {
        const at = Date.now()
        const next: Record<string, number> = {}
        for (const connection of state.connections) {
            const job = state.jobs.find(item => item.connectionId === connection.id)
            // Only a running phase measured in bytes against a fixed total has a time left.
            const phase = job && externalJobIsActive(job) && !externalJobIsPaused(job) && activeJobFraction(job) !== null
                ? `${job.id}:${job.phase}:${job.counters ?? ''}` : undefined
            const items = { done: Number(job?.completedItems), total: Number(job?.totalItems) }
            let estimator = remainingEstimators.get(connection.id)
            if (!estimator) remainingEstimators.set(connection.id, estimator = createRemainingTimeEstimator())
            const left = estimator.update(phase, Number(job?.completedBytes ?? 0), Number(job?.totalBytes ?? 0), at, Number.isFinite(items.done) && items.total > 0 ? items : undefined)
            if (left !== undefined) next[connection.id] = left
        }
        remaining = next
    }

    function schedulePoll(whileBusy = false): void {
        clearTimeout(pollTimer)
        if (destroyed) return
        if (!storageState?.jobs.some(externalJobIsActive) && !(whileBusy && busy)) return
        pollTimer = setTimeout(async () => {
            await refresh(true)
            schedulePoll(whileBusy)
        }, 1200)
    }

    async function onConnected(result: ExternalConnectionResult): Promise<void> {
        busy = false
        adding = false
        const addressChanged = addressConnection !== undefined
        addressConnection = undefined
        const renewed = renewalConnection !== undefined
        renewalConnection = undefined
        if (result.recovery) recoveryKey = result.recovery.key
        await refreshExternalStorageProductionState()
        await refresh()
        schedulePoll()
        if (addressChanged) return
        if (renewed) {
            const job = activeJob(result.connection)
            if (job && externalJobIsPaused(job)) await resumeJob(result.connection, job)
        } else if (result.connection.purpose === 'sync') {
            if (recoveryKey) pendingSyncStart = result.connection
            else await startSync(result.connection)
        }
    }

    /** Re-enters the paused job instead of starting one beside it. */
    async function resumeJob(connection: ExternalConnectionSummary, job: ExternalJobSummary): Promise<void> {
        if (job.kind === 'restore' && job.restoreRequest) {
            await runJob(connection, 'restore', job.restoreRequest)
            return
        }
        busy = true
        try {
            const operation = resumeExternalStorageJob(job)
            schedulePoll(true)
            const result = await operation
            if (result.kind === 'complete' && (job.kind === 'pin-history' || job.kind === 'delete-history')) await loadHistory(connection, false)
            if (result.kind === 'blocked' && !result.job) error = externalErrorMessage(strings, result.error ?? result.cause)
            await refresh(true)
            schedulePoll()
        } catch (reason) { error = externalErrorMessage(strings, reason) }
        finally { busy = false }
    }

    async function confirmRestore(): Promise<boolean> {
        // A device that syncs receives the latest changes first, so its characters match the remote.
        const prepared = await prepareBoundLibraryReplacement().catch(() => undefined)
        if (prepared && !await hasLibraryCharacters()) return true
        const bound = prepared?.bound ?? (await createNativeSyncBindingBridge().state()).target.kind !== 'none'
        return (await alertCheckboxConfirm({
            title: language.lwwSync.restoreTitle,
            description: bound ? language.lwwSync.restoreDescriptionBound : language.lwwSync.restoreDescription,
            checkboxLabel: language.lwwSync.restoreAcknowledge,
            actionLabel: language.lwwSync.restoreAction,
            cancelLabel: strings.cancel,
            requireChecked: true,
        })).confirmed
    }

    async function runJob(
        connection: ExternalConnectionSummary,
        job: 'backup' | 'restore' | 'pin-history' | 'cleanup' | 'check-repository',
        details: {
            snapshotId?: string
            jobId?: string
            restoreAreas?: ExternalRestoreArea[]
        } = {},
    ): Promise<void> {
        if (job === 'restore') {
            busy = true
            const confirmed = await confirmRestore().finally(() => { busy = false })
            if (!confirmed) return
        }
        busy = true
        activeAction = `${job}:${connection.id}`
        try {
            if (job === 'backup' || job === 'cleanup') {
                const operation = requestExternalStorageNow(connection.id, job)
                schedulePoll(true)
                const result = await operation
                if (job === 'backup' && result.kind === 'complete') historyJobs.set(result.job.id, connection.id)
                if (result.kind === 'blocked' && !result.job) {
                    error = externalErrorMessage(strings, result.error ?? result.cause ?? { code: result.reason })
                }
            } else if (job === 'restore') {
                if (!details.snapshotId || !details.restoreAreas) {
                    throw new Error('Missing restore request')
                }
                const operation = requestExternalStorageRestore(
                    connection.id,
                    details.snapshotId,
                    details.restoreAreas,
                )
                schedulePoll(true)
                await operation
            } else {
                const started = await startExternalStorageMaintenanceJob({
                    connectionId: connection.id,
                    kind: job,
                    reason: 'manual',
                    ...details,
                })
                if (job === 'pin-history') {
                    if (started.state === 'succeeded') await loadHistory(connection, false)
                    else historyJobs.set(started.id, connection.id)
                }
            }
            await refresh(true)
            schedulePoll()
        } catch (reason) {
            const message = externalErrorMessage(strings, reason)
            if (job === 'restore') await refresh(true)
            if (job !== 'restore' || !rowShowsStoppedRestore(connection.id, message)) error = message
        } finally {
            busy = false
            activeAction = ''
        }
    }

    /** A restore the app stopped shows its error on the connection row, which the section does not repeat. */
    function rowShowsStoppedRestore(connectionId: string, message: string): boolean {
        const current = storageState?.connections.find(item => item.id === connectionId)
        const latest = current && activeJob(current)
        return !!current && !current.lastError && latest?.state === 'cancelled' && latest.stoppedByApp === true
            && jobLabel(latest) === message
    }

    async function deleteHistory(
        connection: ExternalConnectionSummary,
        item: ExternalHistoryItem,
    ): Promise<void> {
        if (!item.pointId || !item.pointObservation || !item.deletable) return
        busy = true
        try {
            const preparation = await bridge.prepareHistoryDelete(
                connection.id,
                item.pointId,
                item.pointObservation,
            )
            const descriptions = [
                ...(!preparation.sameDevice ? [strings.deleteOtherDeviceConfirm] : []),
                ...(preparation.lastRetained ? [strings.deleteLastRetainedConfirm] : []),
            ]
            if (descriptions.length) {
                const answer = await alertCheckboxConfirm({
                    title: strings.deleteHistoryConfirm,
                    description: descriptions.join(' '),
                    checkboxLabel: strings.deleteHistoryAcknowledge,
                    actionLabel: strings.deleteHistory,
                    cancelLabel: strings.cancel,
                    requireChecked: true,
                })
                if (!answer.confirmed) return
            } else if (!(await alertConfirm(strings.deleteHistoryConfirm))) return
            const operation = requestExternalStorageDeleteHistory(
                connection.id,
                item,
                !preparation.sameDevice,
                preparation.lastRetained,
            )
            schedulePoll(true)
            await operation
            await loadHistory(connection, false)
            await refresh(true)
            schedulePoll()
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function cancelJob(job: ExternalJobSummary): Promise<void> {
        try {
            await bridge.cancelJob(job.id)
            await refresh(true)
            schedulePoll()
        } catch (reason) { error = externalErrorMessage(strings, reason) }
    }

    async function stopRestore(job: ExternalJobSummary): Promise<void> {
        if (busy || !(await alertConfirm(`${strings.stopRestoreTitle}\n${strings.stopRestoreDescription}`))) return
        busy = true
        try {
            await stopExternalStorageRestore(job.id)
            await refresh(true)
        } catch (reason) { error = externalErrorMessage(strings, reason) }
        finally { busy = false }
    }


    async function setAutomaticWork(connection: ExternalConnectionSummary, enabled: boolean): Promise<void> {
        if (!storageState || busy) return
        busy = true
        automaticRequest[connection.id] = enabled
        try {
            await bridge.setAutomaticBackupPaused(connection.id, !enabled)
            await refreshExternalStorageProductionState()
            error = ''
        } catch (reason) { error = externalErrorMessage(strings, reason) }
        finally {
            await refresh(true)
            delete automaticRequest[connection.id]
            busy = false
        }
    }

    async function unlock(): Promise<void> {
        const connection = unlockConnection
        if (!connection || busy) return
        busy = true
        try {
            await bridge.unlockConnection(connection.id, unlockKey)
            unlockKey = ''
            unlockConnection = undefined
            await refreshExternalStorageProductionState()
            await refresh(true)
        } catch (reason) { error = externalErrorMessage(strings, reason) }
        finally { busy = false }
        const job = activeJob(connection)
        if (!unlockConnection && job && externalJobIsPaused(job)) await resumeJob(connection, job)
    }


    function selectedTab(connectionId: string): 'history' | 'quota' {
        return expanded[connectionId] ?? 'history'
    }

    async function openDetails(connection: ExternalConnectionSummary, kind: 'history' | 'quota'): Promise<void> {
        expanded[connection.id] = kind
        try {
            if (kind === 'history') await loadHistory(connection, false)
            if (kind === 'quota') {
                await loadQuota(connection)
            }
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        }
    }

    function loadQuota(connection: ExternalConnectionSummary): Promise<void> {
        quotaRequested.add(connection.id)
        const pending = quotaLoading.get(connection.id)
        if (pending) return pending
        const request = (async () => {
            do {
                quotaRequested.delete(connection.id)
                const [usage] = await Promise.all([bridge.getQuota(connection.id), loadRemoteOnlyFiles(connection)])
                if (!destroyed) quota[connection.id] = usage
            } while (!destroyed && quotaRequested.has(connection.id))
        })().finally(() => { quotaLoading.delete(connection.id); quotaRequested.delete(connection.id) })
        quotaLoading.set(connection.id, request)
        return request
    }

    /** Files only this connection holds, as the residency status counts them. */
    function remoteOnlyFiles(connection: ExternalConnectionSummary): Promise<number> {
        return countConnectionOnlyAssets(connection.id)
    }

    async function loadRemoteOnlyFiles(connection: ExternalConnectionSummary): Promise<void> {
        try {
            remoteOnly[connection.id] = await remoteOnlyFiles(connection)
        } catch {
            remoteOnly[connection.id] = null
        }
    }


    async function loadHistory(connection: ExternalConnectionSummary, append: boolean): Promise<void> {
        if (historyLoading[connection.id]) return
        historyLoading[connection.id] = true
        try {
            let items = append ? history[connection.id] ?? [] : []
            let cursor = append ? historyCursor[connection.id] : undefined
            // The first page covers kept and conflict copies only, so a
            // repository that just holds backups answers it with nothing. Keep
            // reading until this step has something to show.
            for (let step = 0; step < EMPTY_HISTORY_STEPS; step += 1) {
                const page = await bridge.listHistory(connection.id, cursor)
                items = mergeExternalHistoryItems(items, page.items)
                cursor = page.nextCursor
                if (page.items.length > 0 || !cursor) break
            }
            history[connection.id] = items
            historyCursor[connection.id] = cursor
            delete historyError[connection.id]
            if (selectedTab(connection.id) === 'quota') await loadQuota(connection)
        } catch (reason) {
            historyError[connection.id] = externalErrorMessage(strings, reason)
        } finally {
            historyLoading[connection.id] = false
        }
    }


    async function changeRetentionPolicy(
        connection: ExternalConnectionSummary,
        policy: ExternalRetentionPolicy,
    ): Promise<void> {
        busy = true
        try {
            await bridge.setRetentionPolicy(connection.id, policy)
            await refresh(true)
        } catch (failure) {
            error = externalErrorMessage(strings, failure)
        } finally {
            busy = false
        }
    }

    /** A value outside what the repository accepts goes back to the stored one. */
    function commitRetentionLimit(
        connection: ExternalConnectionSummary,
        limit: keyof ExternalRetentionPolicy,
        input: HTMLInputElement,
    ): void {
        const policy = connection.retentionPolicy
        const [lowest, highest] = RETENTION_LIMITS[limit]
        const value = Number(input.value)
        if (!Number.isInteger(value) || value < lowest || value > highest) {
            input.value = String(policy[limit])
            return
        }
        if (value !== policy[limit]) void changeRetentionPolicy(connection, { ...policy, [limit]: value })
    }

    async function commitTransferConcurrency(connection: ExternalConnectionSummary, input: HTMLInputElement): Promise<void> {
        const saved = connection.transferConcurrency
        const value = Number(input.value)
        if (!input.value.trim() || !Number.isInteger(value) || value < 1 || value > 16) {
            input.value = String(saved)
            return
        }
        if (value === saved) return
        busy = true
        try {
            await bridge.setTransferConcurrency(connection.id, value)
            connection.transferConcurrency = value
            await refresh(true)
        } catch (failure) {
            input.value = String(connection.transferConcurrency)
            error = externalErrorMessage(strings, failure)
        } finally {
            busy = false
        }
    }

    async function removeConnection(connection: ExternalConnectionSummary): Promise<void> {
        busy = true
        activeAction = `remove:${connection.id}`
        const check = new AbortController()
        removalCheck = { connectionId: connection.id, controller: check }
        let held: number | undefined
        try {
            held = await Promise.race([
                remoteOnlyFiles(connection),
                new Promise<undefined>(resolve => check.signal.addEventListener('abort', () => resolve(undefined), { once: true })),
            ])
        } catch {} finally {
            removalCheck = null
            busy = false
            activeAction = ''
        }
        if (check.signal.aborted) return
        const remoteOnly = held !== 0
        // The card's own two lines tell apart connections to one service and account.
        const name = [externalConnectionTitle(strings, connection), externalConnectionPlace(strings, connection)].filter(Boolean).join('\n')
        const choice = await alertCheckboxConfirm({
            title: strings.removeTitle,
            description: remoteOnly ? `${name}\n\n${strings.removeDeletes}\n\n${held === undefined ? strings.removeRemoteOnlyUnknown : strings.removeRemoteOnly}` : name,
            checkboxLabel: remoteOnly ? strings.downloadThenRemove : strings.removeAcknowledge,
            actionLabel: strings.remove,
            cancelLabel: strings.cancel,
            requireChecked: !remoteOnly,
        })
        if (!choice.confirmed) return
        const download = remoteOnly && choice.checked
        busy = true
        activeAction = `remove:${connection.id}`
        try {
            if (download) {
                const controller = new AbortController()
                removalDownload = { connectionId: connection.id, controller, cancelling: false }
                try {
                    await downloadRemoteAssets(connection.id, { signal: controller.signal })
                    if (controller.signal.aborted) return
                } catch (reason) {
                    if (!controller.signal.aborted && externalErrorKind(reason) !== 'cancelled') error = strings.downloadFailedKeptConnection
                    return
                } finally {
                    removalDownload = null
                }
            }
            await bridge.removeConnection(connection.id)
            clearExternalProgress(connection.id)
            await refreshExternalStorageProductionState()
            await refresh(true)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            await loadRemoteOnlyFiles(connection)
            busy = false
            activeAction = ''
        }
    }


    async function createConnectionSettings(connection: ExternalConnectionSummary): Promise<void> {
        busy = true
        activeAction = `settings:${connection.id}`
        try {
            connectionSettings = await bridge.beginConnectionSettingsExport(connection.id)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
            activeAction = ''
        }
    }

    function closeRecoveryKey(): void {
        if (!recoveryKey) return
        recoveryKey = ''
        const connection = pendingSyncStart
        pendingSyncStart = undefined
        if (connection) void startSync(connection)
    }

    function closeConnectionSettings(): void {
        connectionSettings = null
    }

    async function exportSnapshot(connectionId: string, snapshotId: string): Promise<void> {
        if (busy || exportRun) return
        const id = crypto.randomUUID()
        exportRun = { id, connectionId }
        const observation = beginExternalProgress(connectionId, 'export')
        busy = true
        try {
            const result = await runWithMobileBackgroundTask('export', async background => {
                const abort = () => { void bridge.cancelExport(id).catch(() => {}) }
                background.signal?.addEventListener('abort', abort, { once: true })
                try {
                    background.signal?.throwIfAborted()
                    const result = await bridge.exportSnapshot(connectionId, snapshotId, id, progress => {
                        if (exportRun?.id === id) exportRun = { id, connectionId, progress }
                        observation.exported(progress)
                        background.progress(measuredTaskPercent(Number(progress.completedBytes), Number(progress.totalBytes)))
                    }, observation.networkChannel())
                    return result.cancelled ? null : result
                } finally { background.signal?.removeEventListener('abort', abort) }
            }, undefined, true)
            observation.finish(result ? 'complete' : 'cancelled')
        } catch (reason) {
            observation.finish(externalProgressFailure(reason))
            if (externalErrorKind(reason) !== 'cancelled') error = externalErrorMessage(strings, reason)
        } finally {
            if (exportRun?.id === id) exportRun = null
            busy = false
        }
    }

    function bytes(value?: string): string {
        return value ? formatRisuNestStorageBytes(Number(value)) : '—'
    }

    function jobLabel(job: ExternalJobSummary): string {
        if (job.state === 'succeeded') return strings.completed
        if (job.state === 'failed') return errorLabel(job.error)
        if (job.state === 'uncertain') return strings.uncertain
        if (externalJobIsPaused(job)) return errorLabel(job.error)
        if (job.state === 'waiting') return strings.waiting
        if (job.state === 'queued') return strings.queued
        if (job.state === 'running') return strings.running
        if (job.state === 'conflict') return strings.resolveRequired
        if (job.stoppedByApp) return errorLabel(job.error)
        return strings.cancelled
    }

    function errorLabel(value?: ExternalJobSummary['error']): string {
        return externalErrorLabel(strings, value)
    }

    /** A connection whose automatic backup is off reports `paused`, and it runs every other job. */
    function connectionUsable(connection: ExternalConnectionSummary): boolean {
        return connection.status === 'ready' || connection.status === 'paused'
    }

    function connectionStatusLabel(status: ExternalConnectionSummary['status']): string {
        if (status === 'ready' || status === 'paused') return strings.statusReady
        if (status === 'reauth-required') return strings.statusReauth
        if (status === 'key-locked') return strings.statusLocked
        return strings.statusError
    }

    function activeJob(connection: ExternalConnectionSummary): ExternalJobSummary | undefined {
        return storageState?.jobs.find(job => job.connectionId === connection.id)
    }

    function unfinishedRestore(job: ExternalJobSummary | undefined): boolean {
        return job?.kind === 'restore' && job.state === 'uncertain'
    }

    // An unfinished restore reports its own error through the connection, which
    // its message replaces.
    function restoreReportsError(connection: ExternalConnectionSummary, job: ExternalJobSummary | undefined): boolean {
        return unfinishedRestore(job) && connection.lastError?.code === job?.error?.code
            && connection.lastError?.reason === job?.error?.reason
    }

    /** A sync target that this start left off, or that stopped, until its switch or sync button starts it again. */
    function syncStopped(connection: ExternalConnectionSummary): boolean {
        const selection = storageState?.selection
        return connection.purpose === 'sync' && selection?.kind === 'external' && selection.connectionId === connection.id && !syncRunning[connection.id]
    }

    // The status button shows the same badge, so the mapping lives with the sync status view.
    function connectionBadge(connection: ExternalConnectionSummary) {
        return externalSyncStatusView(strings, connection, activeJob(connection), externalProgressFor(progress, connection.id), syncStopped(connection), syncCatchingUp.has(connection.id), syncFailures.get(connection.id))
    }

    // A backup registers each pack as it seals it, so its upload has no total until it ends.
    const uploadTotalGrows = externalJobTotalGrows

    function activeJobFraction(job: ExternalJobSummary): number | null {
        return externalJobFraction(job)
    }

    function jobSize(job: ExternalJobSummary): string {
        const size = `${bytes(job.completedBytes)}${job.totalBytes && !uploadTotalGrows(job) ? ` / ${bytes(job.totalBytes)}` : ''}`
        return job.counters ? `${strings.jobCounters[job.counters]} ${size}` : size
    }

    function jobSummary(job: ExternalJobSummary): string {
        if (job.kind === 'check-repository' && job.state === 'succeeded' && job.result?.stopReason === 'expired') return strings.checkExpired
        if (job.kind === 'check-repository' && job.state === 'succeeded' && job.result?.verifiedObjects !== undefined && job.result.verifiedBytes !== undefined) {
            const summary = strings.checkSummary.replace('{0}', job.result.verifiedObjects).replace('{1}', bytes(job.result.verifiedBytes))
            const damaged = Number(job.result.damagedObjects ?? '0')
            return damaged > 0 ? `${summary} ${strings.checkDamaged.replace('{0}', String(damaged))}` : summary
        }
        if (job.kind === 'cleanup' && job.state === 'succeeded' && job.result?.deletedObjects !== undefined && job.result.deletedBytes !== undefined) {
            const summary = strings.cleanupSummary.replace('{0}', job.result.deletedObjects).replace('{1}', bytes(job.result.deletedBytes))
            return job.result.stopReason === 'complete' ? summary : `${summary} ${strings.cleanupPartial}`
        }
        const progress = externalJobProgress(job)
        const size = jobSize(job)
        const state = externalJobIsActive(job) ? strings.jobActive[job.kind] : `${strings.jobKinds[job.kind]} · ${jobLabel(job)}`
        return progress === null ? `${state} · ${size}` : `${state} · ${size} (${Math.round(progress * 100)}%)`
    }


    function when(value?: string): string {
        return value ? new Date(Number(value)).toLocaleString() : '—'
    }

    function providerIcon(providerId: ExternalConnectionSummary['providerId']) {
        if (providerId === 'webdav') return ServerIcon
        if (providerId === 's3') return DatabaseIcon
        if (providerId === 'github_releases' || providerId === 'gitlab_packages') return PackageIcon
        return CloudIcon
    }

    function atLeast(value: string): string {
        return strings.atLeast.replace('{0}', bytes(value))
    }

    $effect(() => {
        if (recoveryKey) recoveryPanel?.focus()
    })

    onMount(async () => {
        stopSyncFailures = subscribeExternalLwwFailures(value => syncFailures = value)
        stopSyncCatchUp = subscribeExternalLwwCatchUp(value => syncCatchingUp = value)
        stopProgress = subscribeExternalProgress(value => {
            const completed = [...value].some(([key, operation]) => operation.visible && operation.state === 'complete' && progress.get(key)?.state === 'running')
            progress = value
            if (completed && !destroyed) void refresh(true)
        })
        stopBindingChanges = subscribeSyncBindingChanges(() => { if (!destroyed) void refresh(true) })
        try {
            const stop = await bridge.onJobStarted(() => {
                if (destroyed) return
                void refresh(true).then(() => { if (!destroyed) schedulePoll() })
            })
            if (destroyed) { stop(); return }
            stopJobEvents = stop
        } catch (reason) { error = externalErrorMessage(strings, reason) }
        if (destroyed) return
        await refresh()
        schedulePoll()
    })
    onDestroy(() => {
        destroyed = true
        removalDownload?.controller.abort()
        removalCheck?.controller.abort()
        clearTimeout(pollTimer)
        stopJobEvents?.()
        stopSyncFailures?.()
        stopSyncCatchUp?.()
        stopProgress?.()
        stopBindingChanges?.()
        if (exportRun) void bridge.cancelExport(exportRun.id).catch(() => {})
    })
</script>


<svelte:window onkeydown={event => { if (event.key === 'Escape') { closeRecoveryKey(); closeConnectionSettings() } }} />

<SettingGroup id="risunest-external-storage" title={strings.title} description={strings.help}>
    {#snippet actions()}
        {#if storageState?.supported && !adding && !renewalConnection && !addressConnection}<SettingButton disabled={busy} onclick={() => adding = true}>{strings.add}</SettingButton>{/if}
        {#if storageState?.supported !== false}<SettingButton variant="secondary" busy={activeAction === 'refresh'} disabled={busy} onclick={refreshWithHistory}>{strings.refresh}</SettingButton>{/if}
    {/snippet}

    {#if !storageState && busy}<p class="placeholder">{strings.loading}</p>
    {:else if storageState && !storageState.supported}<p class="placeholder">{strings.unsupported}</p>
    {:else if adding || renewalConnection || addressConnection}
        <ConnectionForm {addressConnection} {renewalConnection} {strings} onconnected={onConnected} oncancel={() => { adding = false; renewalConnection = undefined; addressConnection = undefined }} onbusychange={value => busy = value} />
    {:else if storageState}
        {#if !storageState.connections.length}
            <div class="blank">
                <span class="blank-icon" aria-hidden="true"><CloudIcon size={20} /></span>
                <p>{strings.noConnections}</p>
            </div>
        {/if}
        {#each storageState.connections as connection (connection.id)}
            {@const job = activeJob(connection)}
            {@const ProviderIcon = providerIcon(connection.providerId)}
            {@const badge = connectionBadge(connection)}
            {@const renewable = connection.status === 'reauth-required' || job?.error?.action === 'reauthenticate'}
            {@const lockable = connection.status === 'key-locked' || job?.error?.action === 'unlock-key'}
            {@const recheckable = job?.state === 'uncertain' && job.kind === 'backup'}
            {@const retryable = !!job && externalJobIsPaused(job) && ['retry', 'wait', 'free-space'].includes(job.error?.action ?? '') && ['backup', 'cleanup', 'restore', 'check-repository', 'pin-history', 'delete-history'].includes(job.kind)}
            {@const remedy = renewable || lockable || recheckable || retryable}
            {@const syncTarget = storageState.selection.kind === 'external' && storageState.selection.connectionId === connection.id}
            {@const syncing = syncTarget && !!syncRunning[connection.id]}
            {@const backedUp = connection.purpose === 'backup' || !!connection.lastBackupAtMs}
            {@const synced = connection.purpose === 'sync' && !!connection.lastSyncAtMs}
            {@const place = externalConnectionPlace(strings, connection)}
            <article class="card">
                <header class="card-head">
                    <span class="provider" aria-hidden="true"><ProviderIcon size={18} /></span>
                    <div class="card-name">
                        <h3 class="card-title">{externalConnectionTitle(strings, connection)}</h3>
                        {#if place}<p class="card-sub">{place}</p>{/if}
                    </div>
                    <div class="card-status"><StatusBadge label={badge.label} tone={badge.tone} /></div>
                </header>

                {#if connection.lastError && !restoreReportsError(connection, job)}<SettingNotice text={errorLabel(connection.lastError)} />{/if}
                {#if syncFailures.has(connection.id)}
                    {@const failure = syncFailures.get(connection.id)}
                    <SettingNotice role="status" text={externalSyncFailureText(strings, failure)} />
                    {#if syncTarget && externalFormatCode(failure) === 'library-format-switch-required'}
                        <div class="actions"><SettingButton disabled={busy} onclick={() => void switchFormat(connection)}>{language.lwwSync.formatSwitch}</SettingButton></div>
                    {/if}
                    {#if ['clockSkew', 'corrupt'].includes(externalErrorKind(failure) ?? '') && supportsExternalLwwNewDevice(connection.id)}
                        <div class="actions">
                            <BindingTargetSwitch target={{kind:'external',connectionId:connection.id}} options={{mode:'new-device'}} label={language.lwwSync.newDeviceAction} onBound={() => refreshExternalStorageProductionState().then(() => refresh())} onError={reason => error = externalErrorMessage(strings, reason)} />
                        </div>
                    {/if}
                {/if}
                {#if job && (!externalJobIsActive(job) || externalJobIsPaused(job)) && job.state !== 'succeeded' && !connection.lastError && !unfinishedRestore(job)}<SettingNotice role="status" tone={job.state === 'cancelled' && !job.stoppedByApp ? 'info' : 'danger'} text={jobLabel(job)} />{/if}

                {#if unfinishedRestore(job)}
                    <SettingNotice role="status" text={strings.restoreUnfinished} />
                {:else if job?.state === 'uncertain'}
                    <SettingNotice role="status" text={strings.publicationDecision} />
                {/if}

                {#if backedUp || synced || job?.state === 'succeeded'}
                    <dl class="kv">
                        {#if synced}<dt>{language.risuNest.serverSync.lastSuccess}</dt><dd>{when(connection.lastSyncAtMs)}</dd>{/if}
                        {#if backedUp}<dt>{strings.lastBackup}</dt><dd>{when(connection.lastBackupAtMs)}</dd>{/if}
                        {#if job && job.state === 'succeeded'}<dt>{strings.progress}</dt><dd role="status" aria-live="polite">{jobSummary(job)}</dd>{/if}
                    </dl>
                {/if}
                {#if job && externalJobIsPaused(job) && job.error?.retryAtMs}<p class="note">{strings.retryAt.replace('{0}', when(job.error.retryAtMs))}</p>{/if}
                <ExternalTransferProgress connectionId={connection.id} {strings} {job} remaining={remaining[connection.id]} catchingUp={syncCatchingUp.has(connection.id)}>
                    {#snippet exportActions()}
                        {#if exportRun?.connectionId === connection.id}
                            <SettingButton variant="secondary" size="sm" onclick={() => exportRun && bridge.cancelExport(exportRun.id)}>{strings.cancel}</SettingButton>
                        {/if}
                    {/snippet}
                </ExternalTransferProgress>

                {#if unlockConnection?.id === connection.id}
                    <div class="unlock">
                        <label class="unlock-field"><span>{strings.recoveryCode}</span><TextInput hideText bind:value={unlockKey} /></label>
                        <div class="actions">
                            <SettingButton disabled={busy || !unlockKey.trim()} onclick={unlock}>{strings.unlock}</SettingButton>
                            <SettingButton variant="secondary" disabled={busy} onclick={() => { unlockConnection = undefined; unlockKey = '' }}>{strings.cancel}</SettingButton>
                        </div>
                    </div>
                {/if}

                <div class="controls">
                    {#if connection.purpose === 'backup'}
                        <SettingToggle showLabel label={strings.automaticBackup} disabled={busy} checked={automaticRequest[connection.id] ?? !connection.automaticBackupPaused} onchange={enabled => setAutomaticWork(connection, enabled)} />
                    {:else}
                        <SettingToggle showLabel label={strings.makeSyncTarget} disabled={busy} checked={syncRequest[connection.id] ?? syncing} onchange={enabled => setSyncBinding(connection, enabled)} />
                    {/if}
                    <div class="actions">
                        {#if renewable}<SettingButton disabled={busy} onclick={() => renewalConnection = connection}>{strings.renew}</SettingButton>{/if}
                        {#if lockable}<SettingButton disabled={busy} onclick={() => unlockConnection = connection}>{strings.unlock}</SettingButton>{/if}
                        {#if recheckable && job}<SettingButton disabled={busy} onclick={() => resumeJob(connection, job)}>{strings.recheckPublication}</SettingButton>{/if}
                        {#if retryable && job}<SettingButton disabled={busy || Number(job.error?.retryAtMs ?? 0) > Date.now()} onclick={() => resumeJob(connection, job)}>{strings.retryAction}</SettingButton>{/if}
                        {#if connection.purpose === 'backup'}<SettingButton variant={remedy ? 'secondary' : 'primary'} busy={activeAction === `backup:${connection.id}`} disabled={busy || (job && externalJobIsActive(job))} onclick={() => runJob(connection, 'backup')}>{strings.runBackup}</SettingButton>
                        {:else if syncTarget}<SettingButton variant={remedy ? 'secondary' : 'primary'} disabled={busy} onclick={() => syncNow(connection)}>{strings.runSync}</SettingButton>{/if}
                        {#if job && externalJobIsActive(job)}<SettingButton variant="secondary" onclick={() => cancelJob(job)}>{strings.cancel}</SettingButton>{/if}
                        {#if job && unfinishedRestore(job)}<SettingButton variant="secondary" disabled={busy} onclick={() => stopRestore(job)}>{strings.stopRestore}</SettingButton>{/if}
                        {#if connection.providerId === 'webdav' || connection.providerId === 's3'}
                            <SettingButton variant="secondary" disabled={busy} onclick={() => addressConnection = connection}>{strings.changeAddress}</SettingButton>
                        {/if}
                    </div>
                </div>

                <div class="setting">
                    <label class="field">
                        <span>{strings.transferConcurrency}</span>
                        <span class="amount">
                            <NumberInput size="sm" className="w-20 text-right tabular-nums" disabled={busy} min={1} max={16} value={connection.transferConcurrency} onChange={event => commitTransferConcurrency(connection, event.currentTarget)} />
                        </span>
                    </label>
                    <p class="help">{strings.transferConcurrencyHelp}</p>
                </div>

                <div class="details">
                    <div class="tabs" role="tablist">
                        {#each (['history', 'quota'] as const) as tab (tab)}
                            <button type="button" role="tab" id="{connection.id}-{tab}-tab" aria-controls="{connection.id}-{tab}-panel" aria-selected={selectedTab(connection.id) === tab} class="tab" onclick={() => openDetails(connection, tab)}>{tab === 'history' ? strings.history : strings.quota}</button>
                        {/each}
                    </div>

                    {#if selectedTab(connection.id) === 'history'}
                        {@const items = history[connection.id] ?? []}
                        <div class="panel" role="tabpanel" id="{connection.id}-history-panel" aria-labelledby="{connection.id}-history-tab">
                            {#if historyLoading[connection.id]}<p class="empty">{strings.loading}</p>
                            {:else if historyError[connection.id]}<SettingNotice role="status" text={historyError[connection.id]} />
                            {:else if items.length === 0}<p class="empty">{connection.purpose === 'sync' ? strings.noSyncHistory : strings.noHistory}</p>{/if}
                            {#if items.length > 0}
                                <ul class="rows">
                                    {#each items as item (item.id)}
                                        {@const usable = item.complete && item.verified}
                                        <li class="item">
                                            <div class="item-head">
                                                <span class="kind" data-kind={item.kind}>{strings.historyKinds[item.kind]}</span>
                                                <span class="item-time">{when(item.createdAtMs)}</span>
                                                {#if item.pinned || item.sameDevice}
                                                    <span class="item-flags">
                                                        {#if item.pinned}<span class="flag"><PinIcon size={12} aria-hidden="true" />{strings.pinned}</span>{/if}
                                                        {#if item.sameDevice}<span class="flag item-device"><MonitorSmartphoneIcon size={12} aria-hidden="true" />{strings.thisDevice}</span>{/if}
                                                    </span>
                                                {/if}
                                            </div>
                                            <div class="item-actions">
                                                <SettingButton size="sm" variant="secondary" disabled={busy || (job && externalJobIsActive(job)) || !usable} onclick={() => beginRestore(connection, item)}>{strings.restore}</SettingButton>
                                                <SettingButton size="sm" variant="quiet" disabled={busy || (job && externalJobIsActive(job)) || !usable} onclick={() => exportSnapshot(connection.id, item.snapshotId)}>{strings.download}</SettingButton>
                                                <SettingButton size="sm" variant="quiet" disabled={busy || !usable} onclick={() => runJob(connection, 'check-repository', { snapshotId: item.snapshotId })}>{strings.check}</SettingButton>
                                                {#if !item.pinned}<SettingButton size="sm" variant="quiet" disabled={busy || (job && externalJobIsActive(job))} onclick={() => runJob(connection, 'pin-history', { snapshotId: item.snapshotId })}>{strings.pin}</SettingButton>{/if}
                                                {#if item.deletable}<SettingButton size="sm" variant="danger" class="ml-auto" disabled={busy} onclick={() => deleteHistory(connection, item)}>{strings.deleteHistory}</SettingButton>{/if}
                                            </div>
                                        </li>
                                    {/each}
                                </ul>
                            {/if}
                            {#if historyCursor[connection.id]}<div class="more"><SettingButton variant="secondary" size="sm" busy={historyLoading[connection.id]} onclick={() => loadHistory(connection, true)}>{strings.loadMore}</SettingButton></div>{/if}
                        </div>
                    {:else}
                        {@const usage = quota[connection.id]}
                        {@const retention = connection.retentionPolicy}
                        <div class="panel usage" role="tabpanel" id="{connection.id}-quota-panel" aria-labelledby="{connection.id}-quota-tab">
                            {#if usage || remoteOnly[connection.id] !== undefined}
                                <dl class="stats">
                                    {#if usage}
                                        {#if usage.storage.providerPhysicalKnown && usage.storage.providerPhysicalBytes !== null}
                                            <div class="stat">
                                                <dt>{strings.usedByService}</dt>
                                                <dd class="stat-value">{bytes(usage.storage.providerPhysicalBytes)}</dd>
                                            </div>
                                        {/if}
                                        <div class="stat">
                                            <dt>{strings.uploadedLowerBound}</dt>
                                            <dd class="stat-value">{usage.storage.observedAtMs === null ? strings.unknownUsage : bytes(usage.storage.locallyUploadedBytesLowerBound)}</dd>
                                            {#if usage.storage.observedAtMs !== null}
                                                <dd class="stat-note">{strings.files.replace('{0}', Number(usage.storage.locallyUploadedObjectCountLowerBound).toLocaleString())}</dd>
                                                <dd class="stat-note">{strings.lastUsageUpdate}: {when(usage.storage.observedAtMs)}</dd>
                                            {/if}
                                        </div>
                                        {#if usage.storage.latestReachable}
                                            <div class="stat">
                                                <dt>{strings.latestReachable}</dt>
                                                <dd class="stat-value">{atLeast(usage.storage.latestReachable.knownDirectBytes)}</dd>
                                            </div>
                                        {/if}
                                    {/if}
                                    {#if remoteOnly[connection.id] !== undefined}
                                        {@const count = remoteOnly[connection.id]}
                                        <div class="stat">
                                            <dt>{strings.remoteOnlyFiles}</dt>
                                            <dd class="stat-value" data-unknown={count === null}>{count === null ? strings.unknownUsage : count.toLocaleString()}</dd>
                                        </div>
                                    {/if}
                                </dl>
                            {/if}
                            {#if usage && usage.buckets.length > 0}
                                <section class="usage-section">
                                    <h4 class="section-title">{strings.requestUsage}</h4>
                                    <ul class="buckets">
                                        {#each usage.buckets as bucket (bucket.id)}
                                            {@const share = Number(bucket.limit) > 0 ? Math.min(1, Number(bucket.used) / Number(bucket.limit)) : 0}
                                            <li class="bucket">
                                                <div class="bucket-line">
                                                    <span class="bucket-name">{strings.quotaBuckets[bucket.id] ?? bucket.id}</span>
                                                    <span class="bucket-count">{strings.requestCount.replace('{0}', Number(bucket.used).toLocaleString()).replace('{1}', Number(bucket.limit).toLocaleString())}</span>
                                                </div>
                                                <div class="meter" data-high={share >= 0.9} aria-hidden="true"><span style:width="{share * 100}%"></span></div>
                                            </li>
                                        {/each}
                                    </ul>
                                </section>
                            {/if}
                            <section class="usage-section">
                                <div class="fields">
                                    <label class="field">
                                        <span>{strings.retentionCount}</span>
                                        <span class="amount">
                                            <NumberInput size="sm" className="w-20 text-right tabular-nums" disabled={busy} min={RETENTION_LIMITS.keepCount[0]} max={RETENTION_LIMITS.keepCount[1]} value={retention.keepCount} onChange={event => commitRetentionLimit(connection, 'keepCount', event.currentTarget)} />
                                        </span>
                                    </label>
                                    <label class="field">
                                        <span>{strings.retentionDays}</span>
                                        <span class="amount">
                                            <NumberInput size="sm" className="w-20 text-right tabular-nums" disabled={busy} min={RETENTION_LIMITS.keepDays[0]} max={RETENTION_LIMITS.keepDays[1]} value={retention.keepDays} onChange={event => commitRetentionLimit(connection, 'keepDays', event.currentTarget)} />
                                            <span class="unit">{strings.retentionDaysUnit}</span>
                                        </span>
                                    </label>
                                </div>
                                <p class="help">{strings.retentionHelp}</p>
                            </section>
                            <section class="usage-section">
                                {#if connection.capabilities.snapshotDiscovery && connection.capabilities.leaseOperations && connection.capabilities.deleteObjects}
                                    <div class="actions">
                                        <SettingButton variant="secondary" busy={activeAction === `cleanup:${connection.id}`} disabled={busy || !connectionUsable(connection) || storageState.jobs.some(job => job.connectionId === connection.id && externalJobIsActive(job))} onclick={() => runJob(connection, 'cleanup')}>{strings.cleanup}</SettingButton>
                                    </div>
                                {/if}
                                <p class="help">{strings.cleanupTrashNotice} {strings.providerCapacityHelp}</p>
                            </section>
                        </div>
                    {/if}
                </div>

                <footer class="foot">
                    <SettingButton variant="secondary" busy={activeAction === `settings:${connection.id}`} disabled={busy} onclick={() => createConnectionSettings(connection)}>{strings.connectionSettings}</SettingButton>
                    <span class="foot-end">
                        {#if removalDownload?.connectionId === connection.id}
                            <SettingButton variant="secondary" disabled={removalDownload.cancelling} onclick={() => { if (removalDownload) { removalDownload.cancelling = true; removalDownload.controller.abort() } }}>{strings.cancel}</SettingButton>
                        {:else if removalCheck?.connectionId === connection.id}
                            <SettingButton variant="secondary" onclick={() => removalCheck?.controller.abort()}>{strings.cancel}</SettingButton>
                        {/if}
                        <SettingButton variant="danger" busy={activeAction === `remove:${connection.id}`} disabled={busy} onclick={() => removeConnection(connection)}>{strings.remove}</SettingButton>
                    </span>
                </footer>
            </article>
        {/each}
    {/if}
    {#if error || pollError}<div class="group-alert"><SettingNotice role="alert" text={error || pollError} /></div>{/if}
</SettingGroup>

{#if recoveryKey}
    <div class="fixed inset-0 z-[1000] flex items-center justify-center bg-black/60 p-4">
        <div
            bind:this={recoveryPanel}
            tabindex="-1"
            role="dialog"
            aria-modal="true"
            aria-labelledby="external-recovery-title"
            class="dialog outline-hidden"
        >
            <h3 id="external-recovery-title" class="text-lg font-bold">{strings.recovery}</h3>
            <p class="text-sm text-subtext">{strings.recoveryNotice}</p>
            <label class="block text-sm">
                <span class="font-medium">{strings.recoveryCode}</span>
                <input readonly class="mt-1 w-full select-all rounded-md border border-darkborderc bg-darkbg p-2 font-mono shadow-xs" value={recoveryKey} />
                <span class="mt-1 block text-[13px] text-subtext">{strings.recoveryCodeHelp}</span>
            </label>
            <div class="actions"><SettingButton onclick={closeRecoveryKey}>{strings.closeRecovery}</SettingButton></div>
        </div>
    </div>
{/if}

{#if connectionSettings}
    {#key connectionSettings.transferId}
        <ConnectionSettingsDialog payload={connectionSettings.payload} {strings}
            onsave={() => bridge.saveConnectionSettingsFile(connectionSettings!.transferId)} onclose={closeConnectionSettings} />
    {/key}
{/if}

<style>
    .placeholder {
        padding: 1rem;
        font-size: 14px;
        color: var(--color-subtext);
    }
    .blank {
        display: grid;
        justify-items: center;
        gap: 0.625rem;
        padding: 1.75rem 1.25rem;
        text-align: center;
    }
    .blank-icon {
        display: grid;
        place-items: center;
        width: 2.75rem;
        height: 2.75rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 50%;
        background: var(--risu-theme-bgcolor);
        color: var(--color-subtext);
    }
    .blank p {
        max-width: 46ch;
        margin: 0;
        font-size: 13px;
        line-height: 1.55;
        color: var(--color-subtext);
    }
    .card {
        display: grid;
        gap: 0.875rem;
        min-width: 0;
        padding: 1rem;
    }
    .card-head {
        display: grid;
        grid-template-columns: auto minmax(0, 1fr);
        align-items: center;
        gap: 0.5rem 0.75rem;
    }
    .card-status {
        grid-column: 2;
        min-width: 0;
    }
    @container (min-width: 36rem) {
        .card-head {
            grid-template-columns: auto minmax(0, 1fr) auto;
        }
        .card-status {
            grid-column: auto;
            justify-self: end;
            max-width: 16rem;
        }
    }
    .provider {
        display: grid;
        place-items: center;
        width: 2.25rem;
        height: 2.25rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
        color: var(--color-subtext);
    }
    .card-name {
        min-width: 0;
    }
    .card-title {
        margin: 0;
        font-size: 15px;
        font-weight: 600;
        line-height: 1.35;
        overflow-wrap: anywhere;
    }
    .card-sub {
        margin: 0.125rem 0 0;
        font-size: 12.5px;
        line-height: 1.4;
        color: var(--color-subtext);
        overflow-wrap: anywhere;
    }
    .kv {
        display: grid;
        grid-template-columns: auto minmax(0, 1fr);
        gap: 0.375rem 1.25rem;
        margin: 0;
        font-size: 13px;
        line-height: 1.45;
    }
    .kv dt {
        color: var(--color-subtext);
        white-space: nowrap;
    }
    .kv dd {
        margin: 0;
        min-width: 0;
        overflow-wrap: anywhere;
        font-variant-numeric: tabular-nums;
    }
    .note {
        margin: 0;
        font-size: 13px;
        color: var(--color-subtext);
    }
    .unlock {
        display: grid;
        gap: 0.625rem;
        padding: 0.75rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
    }
    .unlock-field {
        display: grid;
        gap: 0.25rem;
        font-size: 13px;
    }
    .controls {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        justify-content: space-between;
        gap: 0.75rem 1rem;
    }
    .actions {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 0.5rem;
    }
    .details {
        min-width: 0;
        overflow: hidden;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
    }
    .setting {
        display: grid;
        gap: 0.375rem;
        min-width: 0;
    }
    .setting .help {
        max-width: none;
    }
    .tabs {
        display: flex;
        gap: 0.25rem;
        padding: 0 0.5rem;
        border-bottom: 1px solid var(--risu-theme-darkborderc);
    }
    .tabs:only-child {
        border-bottom: 0;
    }
    .tab {
        margin-bottom: -1px;
        padding: 0.55rem 0.625rem;
        border: 0;
        border-bottom: 2px solid transparent;
        background: transparent;
        font-size: 13px;
        color: var(--color-subtext);
        cursor: pointer;
    }
    .tabs:only-child .tab {
        margin-bottom: 0;
    }
    .tab:hover {
        color: var(--risu-theme-textcolor);
    }
    .tab[aria-selected='true'] {
        border-bottom-color: var(--risu-theme-primary-500);
        color: var(--risu-theme-textcolor);
        font-weight: 600;
    }
    .tab:focus-visible {
        outline: 2px solid var(--risu-theme-selected);
        outline-offset: -2px;
    }
    .panel {
        display: grid;
        gap: 0.75rem;
        min-width: 0;
        padding: 0.875rem;
    }
    .empty {
        margin: 0;
        font-size: 13px;
        color: var(--color-subtext);
    }
    .rows {
        display: grid;
        margin: 0;
        padding: 0;
        list-style: none;
    }
    .item {
        display: grid;
        gap: 0.375rem;
        min-width: 0;
        padding: 0.75rem 0;
    }
    .item:first-child {
        padding-top: 0;
    }
    .item:last-child {
        padding-bottom: 0;
    }
    .item + .item {
        border-top: 1px solid var(--risu-theme-darkborderc);
    }
    .item-head {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 0.375rem 0.5rem;
    }
    .kind {
        padding: 0.1rem 0.5rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 99px;
        background: var(--risu-theme-darkbg);
        font-size: 11.5px;
        font-weight: 600;
        line-height: 1.5;
        color: var(--color-subtext);
    }
    .kind[data-kind='snapshot'] {
        border-color: color-mix(in srgb, var(--risu-theme-primary-500) 45%, transparent);
        background: color-mix(in srgb, var(--risu-theme-primary-500) 12%, var(--risu-theme-bgcolor));
        color: var(--risu-theme-textcolor);
    }
    .kind[data-kind='conflict'] {
        border-color: color-mix(in srgb, var(--risu-theme-danger-400) 50%, transparent);
        background: color-mix(in srgb, var(--risu-theme-danger-400) 10%, var(--risu-theme-bgcolor));
        color: color-mix(in srgb, var(--risu-theme-danger-400) 65%, var(--risu-theme-textcolor));
    }
    .kind[data-kind='recovery-candidate'] {
        border-color: color-mix(in srgb, var(--risu-theme-secondary-500) 45%, transparent);
        background: color-mix(in srgb, var(--risu-theme-secondary-500) 12%, var(--risu-theme-bgcolor));
        color: var(--risu-theme-textcolor);
    }
    .item-time {
        font-size: 14px;
        font-weight: 500;
        font-variant-numeric: tabular-nums;
    }
    .item-flags {
        display: inline-flex;
        flex-wrap: wrap;
        align-items: center;
        row-gap: 0.25rem;
    }
    .flag {
        display: inline-flex;
        align-items: center;
        gap: 0.25rem;
        font-size: 12px;
        line-height: 1.25;
        color: var(--color-subtext);
    }
    .flag + .flag {
        margin-left: 0.5rem;
        padding-left: 0.5rem;
        border-left: 1px solid var(--risu-theme-darkborderc);
    }
    .item-actions {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 0.25rem 0.375rem;
        margin-top: 0.125rem;
    }
    .more {
        display: flex;
        justify-content: center;
        padding-top: 0.75rem;
        border-top: 1px solid var(--risu-theme-darkborderc);
    }
    .usage {
        gap: 1rem;
    }
    .stats {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(min(100%, 10rem), 1fr));
        gap: 0.5rem;
        margin: 0;
    }
    .stat {
        display: grid;
        align-content: start;
        gap: 0.125rem;
        min-width: 0;
        padding: 0.625rem 0.75rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-darkbg);
    }
    .stat dt {
        font-size: 13px;
        line-height: 1.4;
        color: var(--color-subtext);
    }
    .stat dd {
        margin: 0;
        min-width: 0;
        overflow-wrap: anywhere;
    }
    .stat-value {
        font-size: 16px;
        font-weight: 600;
        line-height: 1.35;
        font-variant-numeric: tabular-nums;
    }
    .stat-value[data-unknown='true'] {
        font-weight: 500;
        color: var(--color-subtext);
    }
    .stat-note {
        font-size: 13px;
        line-height: 1.45;
        color: var(--color-subtext);
    }
    .usage-section {
        display: grid;
        gap: 0.625rem;
        min-width: 0;
        padding-top: 1rem;
        border-top: 1px solid var(--risu-theme-darkborderc);
    }
    .usage-section:first-child {
        padding-top: 0;
        border-top: 0;
    }
    .section-title {
        margin: 0;
        font-size: 13px;
        font-weight: 600;
    }
    .buckets {
        display: grid;
        gap: 0.625rem;
        margin: 0;
        padding: 0;
        list-style: none;
    }
    .bucket {
        display: grid;
        gap: 0.3rem;
        min-width: 0;
    }
    .bucket-line {
        display: flex;
        align-items: baseline;
        justify-content: space-between;
        gap: 0.75rem;
        font-size: 13px;
    }
    .bucket-name {
        min-width: 0;
    }
    .bucket-count {
        color: var(--color-subtext);
        font-variant-numeric: tabular-nums;
        white-space: nowrap;
    }
    .meter {
        height: 4px;
        overflow: hidden;
        border-radius: 99px;
        background: var(--risu-theme-darkbutton);
    }
    .meter span {
        display: block;
        height: 100%;
        border-radius: inherit;
        background: var(--risu-theme-primary-500);
    }
    .meter[data-high='true'] span {
        background: var(--risu-theme-danger-400);
    }
    .fields {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        gap: 0.5rem;
    }
    /* The rows share one value column, so inputs line up whether or not a unit follows them. */
    .fields .field {
        grid-column: 1 / -1;
        grid-template-columns: subgrid;
    }
    .field {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        align-items: center;
        gap: 0.75rem;
        font-size: 14px;
    }
    .amount {
        display: inline-flex;
        align-items: center;
        gap: 0.4rem;
    }
    .unit {
        font-size: 13px;
        color: var(--color-subtext);
    }
    .help {
        max-width: 62ch;
        margin: 0;
        font-size: 13px;
        line-height: 1.5;
        color: var(--color-subtext);
    }
    .foot {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 0.5rem;
        padding-top: 0.875rem;
        border-top: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 70%, transparent);
    }
    .foot-end {
        display: flex;
        flex-wrap: wrap;
        gap: 0.5rem;
        margin-left: auto;
    }
    .group-alert {
        padding: 0.75rem 1rem;
    }
    .dialog {
        display: grid;
        gap: 1rem;
        max-height: 90vh;
        max-width: 28rem;
        overflow-y: auto;
        padding: 1.25rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
        color: var(--risu-theme-textcolor);
    }
</style>
