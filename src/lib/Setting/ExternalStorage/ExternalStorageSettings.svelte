<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import QRCode from 'qrcode'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import { alertConfirm } from 'src/ts/alert'
    import { DBState } from 'src/ts/stores.svelte'
    import { getExternalStorageBridge } from 'src/ts/storage/sync/external/bridge'
    import { externalConflictActions, externalJobIsActive, externalJobProgress, mergeExternalHistoryItems } from 'src/ts/storage/sync/external/connection'
    import {
        refreshExternalStorageProductionState,
        requestExternalStorageNow,
        requestExternalStorageRestore,
    } from 'src/ts/storage/sync/external/production'
    import type {
        ExternalConflictSummary,
        ExternalConnectionResult,
        ExternalConnectionSummary,
        ExternalHistoryItem,
        ExternalJobSummary,
        ExternalQuotaSummary,
        ExternalRecoveryMaterial,
        ExternalStorageState,
    } from 'src/ts/storage/sync/external/types'
    import ConnectionForm from './ConnectionForm.svelte'
    import { externalStorageStrings } from './strings'

    const bridge = getExternalStorageBridge()
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
    let quota = $state<Record<string, ExternalQuotaSummary>>({})
    let recovery = $state<ExternalRecoveryMaterial | null>(null)
    let recoveryQr = $state('')
    let pollTimer: ReturnType<typeof setTimeout> | undefined

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
        } catch {
            error = strings.failed
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
    }

    async function runJob(
        connection: ExternalConnectionSummary,
        job: 'backup' | 'sync' | 'restore' | 'pin-history' | 'resolve-conflict',
        details: { snapshotId?: string; conflictId?: string; choice?: 'local' | 'remote' } = {},
    ): Promise<void> {
        if (job === 'restore' && !(await alertConfirm(strings.restore))) return
        busy = true
        try {
            if (job === 'backup' || job === 'sync') {
                const operation = requestExternalStorageNow(connection.id, job)
                schedulePoll(true)
                await operation
            } else if (job === 'restore') {
                if (!details.snapshotId) throw new Error('Missing snapshot identifier')
                const operation = requestExternalStorageRestore(
                    connection.id,
                    details.snapshotId,
                    [
                        'library', 'referencedAssets',
                        ...(connection.scope.deviceSettings ? ['deviceSettings' as const] : []),
                        ...(connection.scope.devicePlugins ? ['devicePlugins' as const] : []),
                    ],
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
        } catch {
            error = strings.failed
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
        } catch {
            error = strings.failed
        } finally {
            busy = false
        }
    }

    async function openDetails(connection: ExternalConnectionSummary, kind: 'history' | 'conflicts' | 'quota'): Promise<void> {
        if (expanded[connection.id] === kind) {
            expanded[connection.id] = ''
            return
        }
        expanded[connection.id] = kind
        try {
            if (kind === 'history') await loadHistory(connection, false)
            if (kind === 'conflicts') conflicts[connection.id] = await bridge.listConflicts(connection.id)
            if (kind === 'quota') quota[connection.id] = await bridge.getQuota(connection.id)
        } catch {
            error = strings.failed
        }
    }

    async function loadHistory(connection: ExternalConnectionSummary, append: boolean): Promise<void> {
        if (historyLoading[connection.id]) return
        historyLoading[connection.id] = true
        try {
            const page = await bridge.listHistory(
                connection.id,
                append ? historyCursor[connection.id] : undefined,
            )
            history[connection.id] = mergeExternalHistoryItems(
                append ? history[connection.id] ?? [] : [],
                page.items,
            )
            historyCursor[connection.id] = page.nextCursor
            error = ''
        } catch {
            error = strings.failed
        } finally {
            historyLoading[connection.id] = false
        }
    }

    async function removeConnection(connection: ExternalConnectionSummary): Promise<void> {
        if (!(await alertConfirm(`${strings.remove}: ${connection.displayName}`))) return
        busy = true
        try {
            await bridge.removeConnection(connection.id)
            await refreshExternalStorageProductionState()
            await refresh(true)
        } catch {
            error = strings.failed
        } finally {
            busy = false
        }
    }

    async function retryConflictPreservation(connection: ExternalConnectionSummary): Promise<void> {
        await runJob(connection, 'sync')
        try {
            conflicts[connection.id] = await bridge.listConflicts(connection.id)
        } catch {
            error = strings.failed
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
        } catch {
            error = strings.failed
        } finally {
            busy = false
        }
    }

    async function saveRecoveryFile(): Promise<void> {
        if (!recovery) return
        try {
            await bridge.saveRecoveryFile(recovery.recoveryId)
        } catch {
            error = strings.failed
        }
    }

    function closeRecovery(): void {
        recovery = null
        recoveryQr = ''
    }

    async function exportSnapshot(connectionId: string, snapshotId: string): Promise<void> {
        try {
            await bridge.exportSnapshot(connectionId, snapshotId)
        } catch {
            error = strings.failed
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
        if (value.action === 'free-space') return strings.freeSpace
        if (value.action === 'wait') return strings.waiting
        if (value.action === 'retry') return strings.retry
        return strings.failed
    }

    function connectionStatusLabel(status: ExternalConnectionSummary['status']): string {
        if (status === 'ready') return strings.statusReady
        if (status === 'paused') return strings.statusPaused
        if (status === 'reauth-required') return strings.statusReauth
        if (status === 'key-locked') return strings.statusLocked
        return strings.statusError
    }

    onMount(async () => {
        await refresh()
        schedulePoll()
    })
    onDestroy(() => clearTimeout(pollTimer))
</script>

<svelte:window onkeydown={event => { if (event.key === 'Escape' && recovery) closeRecovery() }} />

<SettingGroup id="risunest-external-storage" title={strings.title} divide={false}>
    <div class="p-4">
        <div class="flex flex-wrap items-start justify-between gap-3">
            <p class="max-w-2xl text-sm text-textcolor2">{strings.help}</p>
            <div class="flex gap-2">
                {#if storageState?.supported && !adding}<Button size="sm" disabled={busy} onclick={() => adding = true}>{strings.add}</Button>{/if}
                <Button size="sm" styled="outlined" disabled={busy} onclick={() => refresh()}>{strings.refresh}</Button>
            </div>
        </div>

        {#if !storageState && busy}<p class="mt-3 text-sm text-textcolor2">{strings.loading}</p>
        {:else if storageState && !storageState.supported}<p class="mt-3 rounded-md border border-darkborderc bg-bgcolor p-3 text-sm">{strings.unsupported}</p>
        {:else if adding}<div class="mt-4 rounded-lg border border-darkborderc bg-darkbg"><ConnectionForm {strings} onconnected={onConnected} oncancel={() => adding = false} onbusychange={value => busy = value} /></div>
        {:else if storageState}
            {#if !storageState.connections.length}<p class="mt-3 text-sm text-textcolor2">{strings.noConnections}</p>{/if}
            <div class="mt-4 space-y-3">
                {#each storageState.connections as connection (connection.id)}
                    <article class="rounded-lg border border-darkborderc bg-darkbg p-4">
                        <div class="flex flex-wrap items-start justify-between gap-2">
                            <div><h3 class="font-semibold">{connection.displayName}</h3><p class="text-xs text-textcolor2">{connection.endpoint.authority} · {connection.strategy} · {connectionStatusLabel(connection.status)}</p></div>
                            {#if storageState.selection.kind === 'external' && storageState.selection.connectionId === connection.id}<span class="rounded-full bg-selected px-2 py-1 text-xs">{strings.activeSync}</span>{/if}
                        </div>
                        {#if connection.strategy === 'sequential'}<p class="mt-2 text-xs text-textcolor2">{strings.sequentialWarning}</p>{/if}
                        {#if connection.lastError}<p class="mt-2 text-sm text-draculared">{errorLabel(connection.lastError)}</p>{/if}

                        {#each storageState.jobs.filter(job => job.connectionId === connection.id).slice(0, 1) as job (job.id)}
                            <div class="mt-3 rounded-md bg-bgcolor p-2 text-sm" role="status" aria-live="polite">
                                <div class="flex justify-between gap-2"><span>{job.kind}: {jobLabel(job)}</span><span>{bytes(job.completedBytes)}{job.totalBytes ? ` / ${bytes(job.totalBytes)}` : ''}</span></div>
                                {#if externalJobProgress(job) !== null}<div class="mt-1 h-1.5 overflow-hidden rounded bg-darkborderc"><div class="h-full bg-selected" style={`width:${(externalJobProgress(job) ?? 0) * 100}%`}></div></div>{/if}
                                {#if job.state === 'waiting' && job.error?.action === 'retry' && (job.kind === 'backup' || job.kind === 'sync')}<Button className="mt-2 mr-2" size="sm" onclick={() => runJob(connection, job.kind)}>{strings.retry}</Button>{/if}
                                {#if externalJobIsActive(job)}<Button className="mt-2" size="sm" styled="outlined" onclick={async () => { await bridge.cancelJob(job.id); await refresh(true) }}>{strings.cancel}</Button>{/if}
                            </div>
                        {/each}

                        <div class="mt-3 flex flex-wrap gap-2">
                            <Button size="sm" disabled={busy} onclick={() => runJob(connection, 'backup')}>{strings.runBackup}</Button>
                            {#if connection.purpose === 'sync'}<Button size="sm" disabled={busy} onclick={() => runJob(connection, 'sync')}>{strings.runSync}</Button>{/if}
                            {#if connection.purpose === 'sync' && !(storageState.selection.kind === 'external' && storageState.selection.connectionId === connection.id)}<Button size="sm" styled="outlined" disabled={busy} onclick={() => selectSyncTarget(connection)}>{strings.makeSyncTarget}</Button>{/if}
                            <Button size="sm" styled="outlined" onclick={() => openDetails(connection, 'history')}>{strings.history}</Button>
                            <Button size="sm" styled="outlined" onclick={() => openDetails(connection, 'conflicts')}>{strings.conflicts}</Button>
                            <Button size="sm" styled="outlined" onclick={() => openDetails(connection, 'quota')}>{strings.quota}</Button>
                            <Button size="sm" styled="outlined" onclick={() => createRecovery(connection)}>{strings.recovery}</Button>
                            <Button size="sm" styled="danger" disabled={busy} onclick={() => removeConnection(connection)}>{strings.remove}</Button>
                        </div>

                        {#if expanded[connection.id] === 'history'}
                            <div class="mt-3 space-y-2 border-t border-darkborderc pt-3">
                                {#each history[connection.id] ?? [] as item (item.id)}<div class="rounded bg-bgcolor p-2 text-sm"><div class="flex flex-wrap items-center justify-between gap-2"><span>{new Date(Number(item.createdAtMs)).toLocaleString()} · {item.kind} · r{item.logicalRevision}</span><span class="flex flex-wrap gap-1"><Button size="sm" styled="outlined" disabled={!item.complete || !item.verified} onclick={() => runJob(connection, 'restore', { snapshotId: item.id })}>{strings.restore}</Button><Button size="sm" styled="outlined" disabled={!item.complete || !item.verified} onclick={() => exportSnapshot(connection.id, item.id)}>{strings.download}</Button>{#if !item.pinned}<Button size="sm" styled="outlined" onclick={() => runJob(connection, 'pin-history', { snapshotId: item.id })}>{strings.pin}</Button>{/if}</span></div>{#if item.warning}<p class="mt-1 text-xs text-draculared">{item.warning}</p>{/if}</div>{/each}
                                {#if historyCursor[connection.id]}<Button size="sm" styled="outlined" disabled={historyLoading[connection.id]} onclick={() => loadHistory(connection, true)}>{historyLoading[connection.id] ? strings.loading : strings.loadMore}</Button>{/if}
                            </div>
                        {:else if expanded[connection.id] === 'conflicts'}
                            <div class="mt-3 space-y-2 border-t border-darkborderc pt-3">
                                {#each conflicts[connection.id] ?? [] as conflict (conflict.id)}
                                    <div class="rounded bg-bgcolor p-2 text-sm">
                                        <p>{conflict.localLabel} r{conflict.localRevision} ↔ {conflict.remoteRevision === null ? strings.remotePending : `${conflict.remoteLabel} r${conflict.remoteRevision}`}</p>
                                        <p class="mt-1 text-xs text-textcolor2">{conflict.preservation === 'local-only' ? strings.preservationPending : strings.preservationComplete}</p>
                                        <div class="mt-2 flex gap-2">
                                            {#if externalConflictActions(conflict).includes('retry-sync')}
                                                <Button size="sm" disabled={busy} onclick={() => retryConflictPreservation(connection)}>{strings.retryPreservation}</Button>
                                            {:else if externalConflictActions(conflict).includes('keep-local')}
                                                <Button size="sm" onclick={() => runJob(connection, 'resolve-conflict', { conflictId: conflict.id, choice: 'local' })}>{strings.local}</Button>
                                                <Button size="sm" onclick={() => runJob(connection, 'resolve-conflict', { conflictId: conflict.id, choice: 'remote' })}>{strings.remote}</Button>
                                            {/if}
                                        </div>
                                    </div>
                                {/each}
                            </div>
                        {:else if expanded[connection.id] === 'quota' && quota[connection.id]}
                            <div class="mt-3 border-t border-darkborderc pt-3 text-sm">
                                <p>{strings.quota}: {quota[connection.id].storage.providerPhysicalKnown && quota[connection.id].storage.providerPhysicalBytes !== null ? bytes(quota[connection.id].storage.providerPhysicalBytes ?? undefined) : strings.unknownUsage}</p>
                                <p class="text-textcolor2">{strings.uploadedLowerBound}: ≥ {bytes(quota[connection.id].storage.locallyUploadedBytesLowerBound)} · ≥ {quota[connection.id].storage.locallyUploadedObjectCountLowerBound} {strings.objects}</p>
                                {#if quota[connection.id].storage.latestReachable}<p class="text-textcolor2">{strings.latestReachable}: ≥ {bytes(quota[connection.id].storage.latestReachable.knownDirectBytes)} · ≥ {quota[connection.id].storage.latestReachable.knownDirectObjectCount} {strings.objects}</p>{/if}
                                {#each quota[connection.id].buckets as bucket}<p class="text-textcolor2">{bucket.id}: {bucket.used}{bucket.limit ? ` / ${bucket.limit}` : ''} {bucket.unit}</p>{/each}
                            </div>
                        {/if}
                    </article>
                {/each}
            </div>
        {/if}
        {#if error}<p class="mt-3 text-sm text-draculared" role="alert">{error}</p>{/if}
    </div>
</SettingGroup>

{#if recovery}
    <div class="fixed inset-0 z-60 flex items-center justify-center bg-black/65 p-4" role="dialog" aria-modal="true" aria-label={strings.recovery}>
        <div class="max-h-[90vh] max-w-md overflow-y-auto rounded-lg border border-darkborderc bg-bgcolor p-5 text-textcolor">
            <h3 class="text-lg font-bold">{strings.recovery}</h3><p class="mt-2 text-sm text-textcolor2">{strings.recoveryNotice}</p>
            {#if recoveryQr}<img class="mx-auto mt-4 rounded bg-white p-2" src={recoveryQr} alt={strings.showRecovery} />{:else}<p class="mt-3 rounded-md border border-darkborderc bg-darkbg p-3 text-sm">{strings.recoveryFileOnly}</p>{/if}
            <label class="mt-4 block text-sm"><span>{strings.recoveryCode}</span><input readonly class="mt-1 w-full select-all rounded border border-darkborderc bg-darkbg p-2 font-mono" value={recovery.code} /></label>
            <div class="mt-4 flex flex-wrap gap-2"><Button onclick={saveRecoveryFile}>{strings.saveRecovery}</Button><Button styled="outlined" onclick={closeRecovery}>{strings.closeRecovery}</Button></div>
        </div>
    </div>
{/if}
