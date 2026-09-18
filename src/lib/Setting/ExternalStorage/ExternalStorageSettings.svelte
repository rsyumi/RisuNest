<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import QRCode from 'qrcode'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import { alertConfirm, alertNormal } from 'src/ts/alert'
    import { DBState } from 'src/ts/stores.svelte'
    import { getExternalStorageBridge } from 'src/ts/storage/sync/external/bridge'
    import { externalConflictActions, externalJobIsActive, externalJobProgress, mergeExternalHistoryItems } from 'src/ts/storage/sync/external/connection'
    import {
        refreshExternalStorageProductionState,
        requestExternalConflictExport,
        requestExternalStorageNow,
        requestExternalConflictRestore,
        requestExternalStorageResolveConflict,
        requestExternalStorageRestore,
    } from 'src/ts/storage/sync/external/production'
    import type {
        ExternalConflictSummary,
        ExternalConflictCursor,
        ExternalConnectionResult,
        ExternalCapturePolicy,
        ExternalConnectionSummary,
        ExternalHistoryItem,
        ExternalJobSummary,
        ExternalQuotaSummary,
        ExternalRecoveryMaterial,
        ExternalRetentionPolicy,
        ExternalRestoreArea,
        ExternalRestoreSection,
        ExternalStorageState,
    } from 'src/ts/storage/sync/external/types'
    import { externalRestorableSections, externalRestoreAreas } from 'src/ts/storage/sync/external/restoreScope'
    import ConnectionForm from './ConnectionForm.svelte'
    import { externalConnectionTitle, externalErrorMessage, externalStorageStrings } from './strings'

    const bridge = getExternalStorageBridge()
    /** How many empty history pages one request reads past before stopping. */
    const EMPTY_HISTORY_STEPS = 4
    /** What a repository accepts for each retention limit. */
    const RETENTION_LIMITS = { keepCount: [1, 1000], keepDays: [7, 3650] } as const
    const strings = $derived(externalStorageStrings(DBState.db.language))
    let storageState = $state<ExternalStorageState | null>(null)
    let adding = $state(false)
    let busy = $state(false)
    let error = $state('')
    let expanded = $state<Record<string, 'history' | 'conflicts' | 'quota' | ''>>({})
    let history = $state<Record<string, ExternalHistoryItem[]>>({})
    let historyCursor = $state<Record<string, string | undefined>>({})
    let historyLoading = $state<Record<string, boolean>>({})
    let conflicts = $state<Record<string, ExternalConflictSummary[]>>({})
    let conflictCursor = $state<Record<string, ExternalConflictCursor | undefined>>({})
    let quota = $state<Record<string, ExternalQuotaSummary>>({})
    let recovery = $state<ExternalRecoveryMaterial | null>(null)
    let recoveryQr = $state('')
    /** The history entry whose restore scope is open, and what is ticked in it. */
    let restoreScope = $state<{ id: string; sections: ExternalRestoreSection[] } | null>(null)
    let pollTimer: ReturnType<typeof setTimeout> | undefined

    const sectionLabels: Record<ExternalRestoreSection, keyof typeof strings> = {
        hypa: 'hypa',
        'local-plugins': 'devicePlugins',
        'local-settings': 'deviceSettings',
    }
    const sectionHelp: Record<ExternalRestoreSection, keyof typeof strings> = {
        hypa: 'hypaHelp',
        'local-plugins': 'devicePluginsHelp',
        'local-settings': 'deviceSettingsHelp',
    }

    /**
     * A backup that carries nothing beside the library has nothing to choose,
     * so it restores straight away. Otherwise the areas it covers are offered,
     * ticked, because they are what the backup was made to keep.
     */
    function beginRestore(
        connection: ExternalConnectionSummary,
        item: ExternalHistoryItem,
    ): void {
        const sections = externalRestorableSections(item)
        if (sections.length === 0) {
            void runJob(connection, 'restore', {
                snapshotId: item.id,
                restoreAreas: externalRestoreAreas(item, []),
            })
            return
        }
        restoreScope = { id: item.id, sections }
    }

    function toggleRestoreSection(section: ExternalRestoreSection, included: boolean): void {
        if (!restoreScope) return
        const sections = restoreScope.sections.filter(current => current !== section)
        restoreScope = {
            ...restoreScope,
            sections: included ? [...sections, section] : sections,
        }
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
        if (!silent) busy = true
        try {
            storageState = await bridge.getState()
            error = ''
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            if (!silent) busy = false
        }
    }

    function schedulePoll(whileBusy = false): void {
        clearTimeout(pollTimer)
        if (!storageState?.jobs.some(externalJobIsActive) && !(whileBusy && busy)) return
        pollTimer = setTimeout(async () => {
            await refresh(true)
            schedulePoll(whileBusy)
        }, 1200)
    }

    async function onConnected(result: ExternalConnectionResult): Promise<void> {
        busy = false
        adding = false
        if (result.recovery) await displayRecovery(result.recovery)
        await refreshExternalStorageProductionState()
        await refresh()
        schedulePoll()
        // An already existing repository is opened to read what is in it, so
        // its contents are shown without asking for the history tab first.
        if (result.connection.mode === 'existing') {
            expanded[result.connection.id] = 'history'
            await loadHistory(result.connection, false)
        }
    }

    async function runJob(
        connection: ExternalConnectionSummary,
        job: 'backup' | 'sync' | 'restore' | 'pin-history' | 'resolve-conflict',
        details: {
            snapshotId?: string
            conflictId?: string
            choice?: 'local' | 'remote'
            restoreAreas?: ExternalRestoreArea[]
        } = {},
    ): Promise<void> {
        if (job === 'restore' && !(await alertConfirm(strings.restore))) return
        busy = true
        try {
            if (job === 'backup' || job === 'sync') {
                const operation = requestExternalStorageNow(connection.id, job)
                schedulePoll(true)
                await operation
            } else if (job === 'restore') {
                if (!details.snapshotId || !details.restoreAreas) {
                    throw new Error('Missing restore request')
                }
                restoreScope = null
                const operation = requestExternalStorageRestore(
                    connection.id,
                    details.snapshotId,
                    details.restoreAreas,
                )
                schedulePoll(true)
                await operation
            } else if (job === 'resolve-conflict') {
                if (!details.conflictId || !details.choice)
                    throw new Error('Missing conflict decision')
                const operation = requestExternalStorageResolveConflict(
                    connection.id,
                    details.conflictId,
                    details.choice,
                )
                schedulePoll(true)
                await operation
            } else {
                await bridge.startJob({
                    connectionId: connection.id,
                    kind: job,
                    reason: 'manual',
                    ...details,
                })
            }
            await refresh(true)
            schedulePoll()
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function selectSyncTarget(connection: ExternalConnectionSummary): Promise<void> {
        if (!storageState) return
        busy = true
        try {
            const selection = await bridge.setSyncTarget(connection.id, storageState.selection.selectionEpoch)
            storageState = { ...storageState, selection }
            await refreshExternalStorageProductionState()
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function openDetails(connection: ExternalConnectionSummary, kind: 'history' | 'conflicts' | 'quota'): Promise<void> {
        restoreScope = null
        if (expanded[connection.id] === kind) {
            expanded[connection.id] = ''
            return
        }
        expanded[connection.id] = kind
        try {
            if (kind === 'history') await loadHistory(connection, false)
            if (kind === 'conflicts') await loadConflicts(connection, false)
            if (kind === 'quota') quota[connection.id] = await bridge.getQuota(connection.id)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        }
    }

    async function loadConflicts(
        connection: ExternalConnectionSummary,
        append: boolean,
    ): Promise<void> {
        const page = await bridge.listConflicts(
            append ? conflictCursor[connection.id] : undefined,
            50,
        )
        const pageItems = page.conflicts.filter(
            conflict => conflict.connectionId === connection.id,
        )
        conflicts[connection.id] = append
            ? [...(conflicts[connection.id] ?? []), ...pageItems]
            : pageItems
        conflictCursor[connection.id] = page.nextCursor
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
            error = ''
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            historyLoading[connection.id] = false
        }
    }

    async function changeCapturePolicy(
        connection: ExternalConnectionSummary,
        policy: ExternalCapturePolicy,
    ): Promise<void> {
        busy = true
        try {
            await bridge.setCapturePolicy(connection.id, policy)
            await refresh(true)
        } catch (failure) {
            error = externalErrorMessage(strings, failure)
        } finally {
            busy = false
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

    async function removeConnection(connection: ExternalConnectionSummary): Promise<void> {
        if (!(await alertConfirm(`${strings.remove}: ${externalConnectionTitle(strings, connection)}`))) return
        busy = true
        try {
            await bridge.removeConnection(connection.id)
            await refreshExternalStorageProductionState()
            await refresh(true)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function recheckConflict(
        connection: ExternalConnectionSummary,
        conflict: ExternalConflictSummary,
    ): Promise<void> {
        busy = true
        try {
            const refreshed = await bridge.recheckConflict(conflict.id)
            conflicts[connection.id] = (conflicts[connection.id] ?? []).map(item =>
                item.id === refreshed.id ? refreshed : item)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function restoreConflict(
        conflict: ExternalConflictSummary,
        side: 'local' | 'remote',
    ): Promise<void> {
        busy = true
        try {
            await requestExternalConflictRestore(conflict.id, side)
            await refresh(true)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function exportConflict(
        conflict: ExternalConflictSummary,
        side: 'local' | 'remote',
    ): Promise<void> {
        busy = true
        try {
            await requestExternalConflictExport(conflict.id, side)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function deleteConflict(
        connection: ExternalConnectionSummary,
        conflict: ExternalConflictSummary,
    ): Promise<void> {
        if (!(await alertConfirm(strings.deleteConflictConfirm))) return
        busy = true
        try {
            const result = await bridge.deleteConflict(conflict.id, true)
            conflicts[connection.id] = (conflicts[connection.id] ?? []).filter(
                item => item.id !== conflict.id,
            )
            if (result.remotePoint === 'left-remote') alertNormal(strings.remoteDeletePending)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function displayRecovery(material: ExternalRecoveryMaterial): Promise<void> {
        recovery = material
        if (!material.qrPayload) {
            recoveryQr = ''
            return
        }
        try {
            recoveryQr = await QRCode.toDataURL(material.qrPayload, { margin: 2, width: 240 })
        } catch {
            recoveryQr = ''
        }
    }

    async function createRecovery(connection: ExternalConnectionSummary): Promise<void> {
        busy = true
        try {
            await displayRecovery(await bridge.beginRecoveryExport(connection.id))
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function saveRecoveryFile(): Promise<void> {
        if (!recovery) return
        try {
            await bridge.saveRecoveryFile(recovery.recoveryId)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        }
    }

    function closeRecovery(): void {
        recovery = null
        recoveryQr = ''
    }

    async function exportSnapshot(connectionId: string, snapshotId: string): Promise<void> {
        try {
            await bridge.exportSnapshot(connectionId, snapshotId)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        }
    }

    function bytes(value?: string): string {
        if (!value) return '—'
        const amount = BigInt(value)
        const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
        let divisor = 1n
        let index = 0
        while (index < units.length - 1 && amount >= divisor * 1024n) {
            divisor *= 1024n
            index += 1
        }
        if (index === 0) return `${amount} B`
        return `${Number((amount * 10n) / divisor) / 10} ${units[index]}`
    }

    function jobLabel(job: ExternalJobSummary): string {
        if (job.state === 'succeeded') return strings.completed
        if (job.state === 'failed') return errorLabel(job.error)
        if (job.state === 'uncertain') return strings.uncertain
        if (job.state === 'waiting') return strings.waiting
        if (job.state === 'queued') return strings.queued
        if (job.state === 'running') return strings.running
        if (job.state === 'conflict') return strings.resolveRequired
        return strings.cancel
    }

    function errorLabel(value?: ExternalJobSummary['error']): string {
        if (!value) return strings.failed
        if (value.action === 'reauthenticate') return strings.reauthenticate
        if (value.action === 'unlock-key') return strings.unlockKey
        if (value.action === 'resolve-conflict') return strings.resolveRequired
        return externalErrorMessage(strings, value)
    }

    function connectionStatusLabel(status: ExternalConnectionSummary['status']): string {
        if (status === 'ready') return strings.statusReady
        if (status === 'paused') return strings.statusPaused
        if (status === 'reauth-required') return strings.statusReauth
        if (status === 'key-locked') return strings.statusLocked
        return strings.statusError
    }

    function activeJob(connection: ExternalConnectionSummary): ExternalJobSummary | undefined {
        return storageState?.jobs.find(job => job.connectionId === connection.id)
    }

    function connectionTone(connection: ExternalConnectionSummary): 'connected' | 'working' | 'paused' | 'attention' {
        const job = activeJob(connection)
        if (job && externalJobIsActive(job)) return 'working'
        if (connection.status === 'ready') return 'connected'
        if (connection.status === 'paused') return 'paused'
        return 'attention'
    }

    function connectionStatus(connection: ExternalConnectionSummary): string {
        const job = activeJob(connection)
        if (job && externalJobIsActive(job)) return strings.jobActive[job.kind]
        return connectionStatusLabel(connection.status)
    }

    function jobSummary(job: ExternalJobSummary): string {
        const progress = externalJobProgress(job)
        const size = `${bytes(job.completedBytes)}${job.totalBytes ? ` / ${bytes(job.totalBytes)}` : ''}`
        const state = externalJobIsActive(job) ? strings.jobActive[job.kind] : `${strings.jobKinds[job.kind]} · ${jobLabel(job)}`
        return progress === null ? `${state} · ${size}` : `${state} · ${size} (${Math.round(progress * 100)}%)`
    }

    function isSyncTarget(connection: ExternalConnectionSummary): boolean {
        return storageState?.selection.kind === 'external' && storageState.selection.connectionId === connection.id
    }

    function when(value?: string): string {
        return value ? new Date(Number(value)).toLocaleString() : '—'
    }

    function historyLine(item: ExternalHistoryItem): string {
        return [
            when(item.createdAtMs),
            strings.historyKinds[item.kind],
            item.deviceName,
            item.storedBytes ? bytes(item.storedBytes) : undefined,
        ].filter(Boolean).join(' · ')
    }

    function atLeast(value: string): string {
        return strings.atLeast.replace('{0}', bytes(value))
    }

    onMount(async () => {
        await refresh()
        schedulePoll()
    })
    onDestroy(() => clearTimeout(pollTimer))
</script>


<svelte:window onkeydown={event => { if (event.key === 'Escape' && recovery) closeRecovery() }} />

<SettingGroup id="risunest-external-storage" title={strings.title} description={strings.help}>
    {#snippet actions()}
        {#if storageState?.supported && !adding}<SettingButton disabled={busy} onclick={() => adding = true}>{strings.add}</SettingButton>{/if}
        <SettingButton variant="secondary" disabled={busy} onclick={() => refresh()}>{strings.refresh}</SettingButton>
    {/snippet}

    {#if !storageState && busy}<p class="p-4 text-sm text-textcolor2">{strings.loading}</p>
    {:else if storageState && !storageState.supported}<p class="p-4 text-sm text-textcolor2">{strings.unsupported}</p>
    {:else if adding}
        <ConnectionForm {strings} onconnected={onConnected} oncancel={() => adding = false} onbusychange={value => busy = value} />
    {:else if storageState}
        {#if !storageState.connections.length}<p class="p-4 text-sm text-textcolor2">{strings.noConnections}</p>{/if}
        {#each storageState.connections as connection (connection.id)}
            {@const job = activeJob(connection)}
            {@const syncTarget = isSyncTarget(connection)}
            <article class="card">
                <div class="card-head">
                    <div class="min-w-0">
                        <h3 class="card-title">{externalConnectionTitle(strings, connection)}</h3>
                        <p class="card-sub">{[connection.endpoint.authority, connection.endpoint.repositoryHint].join(' · ')}</p>
                        {#if connection.purpose === 'sync'}
                            <p class="card-sub">{strings.sequentialWarning}</p>
                        {/if}
                    </div>
                    <div class="pills">
                        {#if syncTarget}<span class="status border border-darkborderc">{strings.activeSync}</span>{/if}
                        <span class="status border border-darkborderc" data-tone={connectionTone(connection)} aria-live="polite"><span class="status-dot" aria-hidden="true"></span>{connectionStatus(connection)}</span>
                    </div>
                </div>

                {#if connection.lastError}<p class="text-sm text-danger-400">{errorLabel(connection.lastError)}</p>{/if}
                {#if job && !externalJobIsActive(job) && job.state !== 'succeeded' && !connection.lastError}<p class="text-sm text-danger-400" role="status">{jobLabel(job)}</p>{/if}

                <dl class="kv">
                    {#if connection.purpose === 'sync'}<dt>{strings.lastSync}</dt><dd>{when(connection.lastSyncAtMs)}</dd>{/if}
                    <dt>{strings.lastBackup}</dt><dd>{when(connection.lastBackupAtMs)}</dd>
                    {#if job && (externalJobIsActive(job) || job.state === 'succeeded')}<dt>{strings.progress}</dt><dd role="status" aria-live="polite">{jobSummary(job)}</dd>{/if}
                </dl>
                {#if job && externalJobIsActive(job)}
                    {@const progress = externalJobProgress(job)}
                    <div class="thin" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow={progress === null ? undefined : Math.round(progress * 100)}>
                        <i class={progress === null ? 'pulse' : ''} style={`width:${progress === null ? 100 : progress * 100}%`}></i>
                    </div>
                {/if}

                {#if connection.capturePolicy}
                    {@const policy = connection.capturePolicy}
                    <h4 class="policy-title">{strings.scope}</h4>
                    <p class="policy-note">{strings.scopeHelp}</p>
                    <p class="policy-row fixed"><span>{strings.library}</span><span class="value">{strings.included}</span></p>
                    <label class="policy-row"><input type="checkbox" disabled={busy} checked={policy.hypa} onchange={event => changeCapturePolicy(connection, { ...policy, hypa: event.currentTarget.checked })} /><span>{strings.hypa}</span></label>
                    <label class="policy-row"><input type="checkbox" disabled={busy} checked={policy.localPlugins} onchange={event => changeCapturePolicy(connection, { ...policy, localPlugins: event.currentTarget.checked })} /><span>{strings.devicePlugins}</span></label>
                    <label class="policy-row"><input type="checkbox" disabled={busy} checked={policy.localSettings} onchange={event => changeCapturePolicy(connection, { ...policy, localSettings: event.currentTarget.checked })} /><span>{strings.deviceSettings}</span></label>
                {/if}

                <div class="actions">
                    {#if job && job.state === 'waiting' && job.error?.action === 'retry' && (job.kind === 'backup' || job.kind === 'sync')}
                        <SettingButton disabled={busy} onclick={() => runJob(connection, job.kind)}>{strings.retryAction}</SettingButton>
                    {/if}
                    <SettingButton disabled={busy || (job && externalJobIsActive(job))} onclick={() => runJob(connection, 'backup')}>{strings.runBackup}</SettingButton>
                    {#if connection.purpose === 'sync'}<SettingButton disabled={busy || (job && externalJobIsActive(job))} onclick={() => runJob(connection, 'sync')}>{strings.runSync}</SettingButton>{/if}
                    {#if connection.purpose === 'sync' && !syncTarget}<SettingButton variant="secondary" disabled={busy} onclick={() => selectSyncTarget(connection)}>{strings.makeSyncTarget}</SettingButton>{/if}
                    {#if job && externalJobIsActive(job)}<SettingButton variant="secondary" onclick={async () => { await bridge.cancelJob(job.id); await refresh(true) }}>{strings.cancel}</SettingButton>{/if}
                </div>

                <div class="tabs" role="tablist">
                    {#each (['history', 'conflicts', 'quota'] as const) as tab (tab)}
                        <button type="button" role="tab" aria-selected={expanded[connection.id] === tab} class="tab" onclick={() => openDetails(connection, tab)}>{tab === 'history' ? strings.history : tab === 'conflicts' ? strings.conflicts : strings.quota}</button>
                    {/each}
                </div>

                {#if expanded[connection.id] === 'history'}
                    <div class="list" role="tabpanel">
                        {#each history[connection.id] ?? [] as item (item.id)}
                            <div class="item">
                                <div class="line">
                                    <span>{historyLine(item)}</span>
                                    <span class="item-actions">
                                        <SettingButton variant="secondary" disabled={!item.complete || !item.verified} onclick={() => beginRestore(connection, item)}>{strings.restore}</SettingButton>
                                        <SettingButton variant="secondary" disabled={!item.complete || !item.verified} onclick={() => exportSnapshot(connection.id, item.id)}>{strings.download}</SettingButton>
                                        {#if item.pinned}<SettingButton variant="secondary" disabled>{strings.pinned}</SettingButton>
                                        {:else}<SettingButton variant="secondary" onclick={() => runJob(connection, 'pin-history', { snapshotId: item.id })}>{strings.pin}</SettingButton>{/if}
                                    </span>
                                </div>
                                {#if restoreScope && restoreScope.id === item.id}
                                    {@const chosen = restoreScope.sections}
                                    <div class="scope">
                                        <h4 class="policy-title">{strings.chooseRestore}</h4>
                                        <p class="policy-row fixed"><span>{strings.library}</span><span class="value">{strings.included}</span></p>
                                        {#each externalRestorableSections(item) as section (section)}
                                            <label class="policy-row"><input type="checkbox" disabled={busy} checked={chosen.includes(section)} onchange={event => toggleRestoreSection(section, event.currentTarget.checked)} /><span>{strings[sectionLabels[section]]}</span></label>
                                            <p class="policy-note">{strings[sectionHelp[section]]}</p>
                                        {/each}
                                        <div class="actions">
                                            <SettingButton disabled={busy} onclick={() => runJob(connection, 'restore', { snapshotId: item.id, restoreAreas: externalRestoreAreas(item, chosen) })}>{strings.restore}</SettingButton>
                                            <SettingButton variant="secondary" disabled={busy} onclick={() => { restoreScope = null }}>{strings.cancel}</SettingButton>
                                        </div>
                                    </div>
                                {/if}
                                {#if item.warning}<p class="text-xs text-danger-400">{item.warning}</p>{/if}
                            </div>
                        {/each}
                        {#if historyCursor[connection.id]}<div><SettingButton variant="secondary" busy={historyLoading[connection.id]} onclick={() => loadHistory(connection, true)}>{strings.loadMore}</SettingButton></div>{/if}
                    </div>
                {:else if expanded[connection.id] === 'conflicts'}
                    <div class="list" role="tabpanel">
                        {#each conflicts[connection.id] ?? [] as conflict (conflict.id)}
                            {@const pending = !conflict.remotePointConfirmed}
                            {@const actions = externalConflictActions(conflict)}
                            <div class="conflict" class:info={pending}>
                                <strong>{conflict.resolved ? strings.conflictResolved : pending ? strings.remotePendingTitle : strings.conflictTitle}</strong>
                                <p>{conflict.resolved ? strings.conflictResolvedHelp : pending ? strings.preservationPending : strings.preservationComplete}</p>
                                <dl class="kv">
                                    <dt>{strings.thisDevice}</dt><dd>{conflict.localRevision}{#if !conflict.localAvailable} · {strings.copyUnavailable}{/if}</dd>
                                    <dt>{strings.repositorySide}</dt><dd>{conflict.remoteRevision}{#if pending} · {strings.remotePending}{:else if !conflict.remoteAvailable} · {strings.copyUnavailable}{/if}</dd>
                                </dl>
                                <div class="actions">
                                    {#if actions.includes('retry-sync')}
                                        <SettingButton disabled={busy} onclick={() => recheckConflict(connection, conflict)}>{strings.retryPreservation}</SettingButton>
                                    {/if}
                                    {#if actions.includes('keep-local')}
                                        <SettingButton onclick={() => runJob(connection, 'resolve-conflict', { conflictId: conflict.id, choice: 'local' })}>{strings.local}</SettingButton>
                                    {/if}
                                    {#if actions.includes('use-remote')}
                                        <SettingButton onclick={() => runJob(connection, 'resolve-conflict', { conflictId: conflict.id, choice: 'remote' })}>{strings.remote}</SettingButton>
                                    {/if}
                                    {#if conflict.localAvailable}
                                        <SettingButton variant="secondary" disabled={busy} onclick={() => restoreConflict(conflict, 'local')}>{strings.restoreLocalCopy}</SettingButton>
                                        <SettingButton variant="secondary" disabled={busy} onclick={() => exportConflict(conflict, 'local')}>{strings.exportLocalCopy}</SettingButton>
                                    {/if}
                                    {#if conflict.remoteAvailable}
                                        <SettingButton variant="secondary" disabled={busy} onclick={() => restoreConflict(conflict, 'remote')}>{strings.restoreRemoteCopy}</SettingButton>
                                        <SettingButton variant="secondary" disabled={busy} onclick={() => exportConflict(conflict, 'remote')}>{strings.exportRemoteCopy}</SettingButton>
                                    {/if}
                                    <SettingButton variant="secondary" disabled={busy} onclick={() => deleteConflict(connection, conflict)}>{strings.deleteConflict}</SettingButton>
                                </div>
                            </div>
                        {/each}
                        {#if conflictCursor[connection.id]}<div><SettingButton variant="secondary" onclick={() => loadConflicts(connection, true)}>{strings.loadMore}</SettingButton></div>{/if}
                    </div>
                {:else if expanded[connection.id] === 'quota'}
                    {@const usage = quota[connection.id]}
                    {@const retention = connection.retentionPolicy}
                    <div class="usage" role="tabpanel">
                        {#if usage}
                            <dl class="kv">
                                <dt>{strings.usedByService}</dt>
                                <dd>{#if usage.storage.providerPhysicalKnown && usage.storage.providerPhysicalBytes !== null}{bytes(usage.storage.providerPhysicalBytes ?? undefined)}{:else}{strings.unknownUsage} <span class="text-textcolor2">({strings.unknownUsageHelp})</span>{/if}</dd>
                                <dt>{strings.uploadedLowerBound}</dt>
                                <dd>{atLeast(usage.storage.locallyUploadedBytesLowerBound)} · {strings.files.replace('{0}', usage.storage.locallyUploadedObjectCountLowerBound)}</dd>
                                {#if usage.storage.latestReachable}
                                    <dt>{strings.latestReachable}</dt>
                                    <dd>{atLeast(usage.storage.latestReachable.knownDirectBytes)}</dd>
                                {/if}
                            </dl>
                            {#if usage.buckets.length > 0}
                                <p>{strings.requestUsage}</p>
                                <dl class="kv">
                                    {#each usage.buckets as bucket (bucket.id)}<dt>{bucket.id}</dt><dd>{bucket.used} / {bucket.limit} {bucket.unit}</dd>{/each}
                                </dl>
                            {/if}
                        {/if}
                        <label class="policy-row fixed">
                            <span>{strings.retentionCount}</span>
                            <span class="amount">
                                <input type="number" class="number" disabled={busy} inputmode="numeric" min={RETENTION_LIMITS.keepCount[0]} max={RETENTION_LIMITS.keepCount[1]} value={retention.keepCount} onchange={event => commitRetentionLimit(connection, 'keepCount', event.currentTarget)} />
                            </span>
                        </label>
                        <label class="policy-row fixed">
                            <span>{strings.retentionDays}</span>
                            <span class="amount">
                                <input type="number" class="number" disabled={busy} inputmode="numeric" min={RETENTION_LIMITS.keepDays[0]} max={RETENTION_LIMITS.keepDays[1]} value={retention.keepDays} onchange={event => commitRetentionLimit(connection, 'keepDays', event.currentTarget)} />
                                <span class="value">{strings.retentionDaysUnit}</span>
                            </span>
                        </label>
                    </div>
                {/if}

                <div class="foot">
                    <SettingButton variant="secondary" onclick={() => createRecovery(connection)}>{strings.recovery}</SettingButton>
                    <SettingButton variant="danger" disabled={busy} onclick={() => removeConnection(connection)}>{strings.remove}</SettingButton>
                </div>
            </article>
        {/each}
    {/if}
    {#if error}<p class="px-4 pb-4 text-sm text-danger-400" role="alert">{error}</p>{/if}
</SettingGroup>

{#if recovery}
    <div class="fixed inset-0 z-60 flex items-center justify-center bg-black/65 p-4" role="dialog" aria-modal="true" aria-label={strings.recovery}>
        <div class="dialog">
            <h3 class="text-lg font-bold">{strings.recovery}</h3>
            <p class="text-sm text-textcolor2">{strings.recoveryNotice}</p>
            {#if recoveryQr}<img class="mx-auto rounded bg-white p-2" src={recoveryQr} alt={strings.showRecovery} />
            {:else}<p class="rounded-md border border-darkborderc bg-darkbg p-3 text-sm">{strings.recoveryFileOnly}</p>{/if}
            <label class="block text-sm">
                <span class="font-medium">{strings.recoveryCode}</span>
                <input readonly class="mt-1 w-full select-all rounded border border-darkborderc bg-darkbg p-2 font-mono" value={recovery.code} />
                <span class="mt-1 block text-[13px] text-textcolor2">{strings.recoveryCodeHelp}</span>
            </label>
            <div class="actions"><SettingButton onclick={saveRecoveryFile}>{strings.saveRecovery}</SettingButton><SettingButton variant="secondary" onclick={closeRecovery}>{strings.closeRecovery}</SettingButton></div>
        </div>
    </div>
{/if}

<style>
    .card {
        display: grid;
        gap: 0.75rem;
        padding: 1rem;
        min-width: 0;
    }
    .card-head {
        display: flex;
        flex-wrap: wrap;
        align-items: flex-start;
        justify-content: space-between;
        gap: 0.5rem 1rem;
    }
    .card-title {
        margin: 0;
        font-size: 0.9375rem;
        font-weight: 600;
        overflow-wrap: anywhere;
    }
    .card-sub {
        margin: 0.15rem 0 0;
        font-size: 0.8125rem;
        color: var(--risu-theme-textcolor2);
        overflow-wrap: anywhere;
    }
    .pills {
        display: flex;
        flex-wrap: wrap;
        gap: 0.4rem;
    }
    .policy-title {
        margin: 0.25rem 0 0;
        font-size: 0.875rem;
        font-weight: 600;
    }
    .policy-note {
        margin: 0;
        font-size: 0.8125rem;
        line-height: 1.45;
        color: var(--risu-theme-textcolor2);
    }
    .policy-row {
        display: flex;
        align-items: center;
        gap: 0.5rem;
        margin: 0;
        font-size: 0.875rem;
    }
    .policy-row.fixed {
        justify-content: space-between;
    }
    .policy-row .value {
        color: var(--risu-theme-textcolor2);
    }
    .status {
        display: inline-flex;
        align-items: center;
        gap: 0.5rem;
        border-radius: 99px;
        padding: 0.35rem 0.75rem;
        font-size: 0.75rem;
        white-space: nowrap;
    }
    .status[data-tone="attention"] {
        color: var(--risu-theme-danger-400);
        border-color: color-mix(in srgb, var(--risu-theme-danger-400) 50%, transparent);
    }
    .status-dot {
        width: 0.45rem;
        height: 0.45rem;
        border-radius: 50%;
        background: currentColor;
        opacity: 0.35;
    }
    .status[data-tone="connected"] .status-dot {
        background: var(--risu-theme-success-500);
        opacity: 1;
    }
    .status[data-tone="attention"] .status-dot {
        opacity: 1;
    }
    .status[data-tone="paused"] .status-dot {
        background: transparent;
        box-shadow: inset 0 0 0 1.5px currentColor;
        opacity: 0.7;
    }
    .status[data-tone="working"] .status-dot {
        background: var(--risu-theme-primary-500);
        opacity: 1;
        animation: pulse 1.5s ease-in-out infinite;
    }
    .usage {
        display: grid;
        gap: 0.4rem;
    }
    .amount {
        display: flex;
        align-items: center;
        gap: 0.35rem;
    }
    .number {
        width: 5rem;
        padding: 0.25rem 0.4rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.375rem;
        background: var(--risu-theme-darkbg);
        color: var(--risu-theme-textcolor);
        font-size: 0.875rem;
        font-variant-numeric: tabular-nums;
        text-align: right;
    }
    .number:disabled {
        opacity: 0.6;
    }
    .kv {
        display: grid;
        grid-template-columns: auto minmax(0, 1fr);
        gap: 0.25rem 1rem;
        margin: 0;
        font-size: 0.8125rem;
    }
    .kv dt {
        color: color-mix(in srgb, var(--risu-theme-textcolor) 60%, transparent);
    }
    .kv dd {
        margin: 0;
        min-width: 0;
        overflow-wrap: anywhere;
        font-variant-numeric: tabular-nums;
    }
    .thin {
        height: 4px;
        border-radius: 99px;
        background: color-mix(in srgb, var(--risu-theme-textcolor) 8%, transparent);
        overflow: hidden;
    }
    .thin i {
        display: block;
        height: 100%;
        background: linear-gradient(90deg, #22c8c6, var(--risu-theme-primary-500));
        transition: width 0.3s;
    }
    .thin i.pulse {
        animation: pulse 1.4s ease-in-out infinite;
    }
    .actions,
    .foot,
    .item-actions {
        display: flex;
        flex-wrap: wrap;
        gap: 0.5rem;
    }
    .foot {
        padding-top: 0.75rem;
        border-top: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 55%, transparent);
    }
    .tabs {
        display: flex;
        gap: 0.25rem;
        border-bottom: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 55%, transparent);
    }
    .tab {
        padding: 0.4rem 0.75rem;
        margin-bottom: -1px;
        border: 0;
        border-bottom: 2px solid transparent;
        background: transparent;
        font-size: 0.8125rem;
        color: var(--risu-theme-textcolor2);
        cursor: pointer;
    }
    .tab:hover {
        color: var(--risu-theme-textcolor);
    }
    .tab[aria-selected="true"] {
        color: var(--risu-theme-textcolor);
        border-bottom-color: var(--risu-theme-textcolor);
    }
    .tab:focus-visible {
        outline: 2px solid var(--risu-theme-selected);
        outline-offset: -2px;
    }
    .list {
        display: grid;
        gap: 0.5rem;
    }
    .item {
        display: grid;
        gap: 0.35rem;
        padding: 0.6rem 0.75rem;
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
        font-size: 0.8125rem;
    }
    .line {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        justify-content: space-between;
        gap: 0.5rem;
    }
    .scope {
        display: grid;
        gap: 0.35rem;
        padding-top: 0.5rem;
        border-top: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 55%, transparent);
    }
    .conflict {
        display: grid;
        gap: 0.5rem;
        padding: 0.85rem 1rem;
        border: 1px solid color-mix(in srgb, var(--risu-theme-danger-400) 45%, transparent);
        border-left-width: 3px;
        border-radius: 0.5rem;
        background: color-mix(in srgb, var(--risu-theme-danger-400) 6%, transparent);
        font-size: 0.875rem;
    }
    .conflict.info {
        border-color: var(--risu-theme-darkborderc);
        background: var(--risu-theme-bgcolor);
    }
    .conflict strong {
        font-weight: 600;
    }
    .conflict p {
        margin: 0;
        font-size: 0.8125rem;
        line-height: 1.45;
        opacity: 0.85;
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
    @keyframes pulse {
        50% {
            opacity: 0.3;
        }
    }
    @media (prefers-reduced-motion: reduce) {
        .status-dot,
        .thin i.pulse {
            animation: none;
        }
    }
</style>
