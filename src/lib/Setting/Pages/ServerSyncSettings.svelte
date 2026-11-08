<script lang="ts">
    import { onMount, onDestroy } from 'svelte'
    import { CloudDownloadIcon, HardDriveIcon, LoaderCircleIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { alertActionConfirm, alertCheckboxConfirm, alertConfirm } from 'src/ts/alert'
    import { isTauri } from 'src/ts/platform'
    import { serverSyncErrorMessage, writerRecoveryCodes, usedRegistrationCodes } from 'src/ts/storage/sync/serverSyncErrorMessage'
    import { serverSyncStatusView } from 'src/ts/storage/sync/syncStatusView'
    import { completeServerSyncBinding, connectServerSync, disconnectServerSync, dismissServerSyncConnectionFailure, holdServerSync, retryServerSync, switchServerSyncLibraryFormat, getServerSyncController, getServerSyncCacheUsage, cleanupServerSyncCache, type ServerSyncCacheUsage } from 'src/ts/storage/sync/serverSyncProduction'
    import { parseServerRegistration } from 'src/ts/storage/sync/serverSyncRegistration'
    import { serverRegistrationInbox } from 'src/ts/storage/sync/serverSyncRegistrationInbox'
    import { canScanServerRegistration, createServerQrScanner } from 'src/ts/storage/sync/serverSyncQr'
    import { isQrScanCancelled } from 'src/ts/ui/qrScanner'
    import { getAssetResidencyStatus, setAssetResidencyPolicy, evictLocalAssets, cancelLocalAssetCleanup, cancelAssetResidencyOperation, type AssetResidencyPolicy, type AssetResidencyStatus } from 'src/ts/storage/sync/serverAssetResidency'
    import { assetCleanupProgress, isAssetCleanupRunning } from 'src/ts/storage/sync/serverAssetCleanupProgress'
    import { describeBlockedReason } from 'src/ts/storage/sync/blockedReasonText'
    import type { ServerConfig } from 'src/ts/storage/sync/serverSync'
    import { formatRisuNestStorageBytes as bytes } from 'src/ts/storage/risuNestStorageDashboard'
    import { serverSyncProgressView, serverSyncRoutineView } from 'src/ts/storage/sync/serverSyncProgress'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import TransferProgress from '../RisuNest/TransferProgress.svelte'
    import SettingNotice from '../RisuNest/SettingNotice.svelte'
    import StatusBadge from '../RisuNest/StatusBadge.svelte'

    interface Props {
        connectTarget?: (config: ServerConfig, newDevice: boolean, policy?: AssetResidencyPolicy) => Promise<unknown>
        /** `onboarding` drops the section heading and panel, which the onboarding screen provides. */
        tone?: 'settings' | 'onboarding'
    }

    let { connectTarget = connectServerSync, tone = 'settings' }: Props = $props()

    const copy = language.risuNest.serverSync
    /** How long disconnecting waits for the file check before it asks without the check's answer. */
    const SERVER_ONLY_CHECK_MS = 3_000
    const controller = getServerSyncController()
    const scanner = createServerQrScanner()
    let view = $state(controller.snapshot())
    let code = $state('')
    let candidate = $state<ServerConfig | undefined>()
    let operating = $state(false)
    let cleaning = $derived(isAssetCleanupRunning($assetCleanupProgress))
    let busy = $derived(operating || cleaning)
    let cleanupDetailsOpen = $state(false)
    let scanning = $state(false)
    let failure = $state('')
    let residency = $state<AssetResidencyStatus | undefined>()
    // Reading the breakdown can outlast an operation started meanwhile, so only the latest read is applied.
    let residencyRead = 0
    let residencyFailed = $state(false)
    let cache = $state<ServerSyncCacheUsage | undefined>()
    let downloading = $state(false)
    let checkingFiles = $state(false)
    let codeOpen = $state(false)
    let policy = $state<AssetResidencyPolicy>('full')
    let pending = $state<number | undefined>()
    let now = $state(Date.now())
    let detailsOpen = $state(false)
    let dispose = () => {}
    let inboxDispose = () => {}
    let stopWatching = () => {}
    let stopCleanup = () => {}
    let mounted = false
    // Sync stays stopped by the collision while another connection attempt fails.
    let writerRecovery = $derived(writerRecoveryCodes.includes(view.error) || writerRecoveryCodes.includes(view.blockedCode))
    let connected = $derived(!!view.status.configured && !!view.status.bound)
    // The connection status carries the stored policy, so the choices do not wait for the breakdown.
    let storedPolicy = $derived(view.status.assetPolicy ?? residency?.policy)
    // A connected device keeps its registration input folded until a code is needed again.
    let codeShown = $derived(!view.status.bound || !!view.error || !!failure || !!candidate || codeOpen)
    let status = $derived(serverSyncStatusView(view))
    // Automatic sync shows one bar as soon as it has anything to move, and keeps it finished for a moment.
    let shown = $derived(view.progress ?? view.finished)
    let summary = $derived(shown?.mode === 'routine' ? serverSyncRoutineView(shown, copy, !view.progress) : undefined)
    // Connecting and downloading every asset show their steps; a short check finishes before they would appear.
    let progress = $derived(!shown ? undefined
        : shown.mode === 'routine' ? summary && serverSyncProgressView(shown, copy, shown.endedAt ?? now)
            : now - shown.startedAt >= 600 ? serverSyncProgressView(shown, copy, now) : undefined)
    // One place shows the panel: the asset download that started it, the server being connected, or the connection.
    let progressPlace = $derived(!progress ? undefined : downloading ? 'assets' : candidate ? 'candidate' : view.status.bound ? 'bound' : 'incomplete')
    // Read again when the connection settles, an attempt ends or the error changes.
    let pendingKey = $derived(`${connected && !view.running && !view.progress}:${view.error}`)
    let residencyOptions = $derived([
        { value: 'full' as AssetResidencyPolicy, label: copy.residency.full },
        { value: 'remote' as AssetResidencyPolicy, label: copy.residency.remote },
    ])
    const errorCode = (error: unknown) => typeof error === 'object' && error && 'code' in error ? String(error.code) : ''
    const message = (error: unknown) => serverSyncErrorMessage(error, tone === 'onboarding', !!view.status.bound)
    async function readResidency() {
        const read = ++residencyRead
        residencyFailed = false
        try {
            const next = await getAssetResidencyStatus()
            if (read === residencyRead) residency = next
        } catch (error) {
            if (read !== residencyRead) return
            residencyFailed = true
            throw error
        }
    }
    async function readCache() { cache = await getServerSyncCacheUsage() }
    // A policy change, download or cleanup answers with the breakdown it leaves, which is shown without reading it again.
    async function refresh(known?: AssetResidencyStatus | void) {
        if (known) { residencyRead++; residencyFailed = false; residency = known }
        await controller.ensureStatus(); await Promise.all([known ? undefined : readResidency(), readCache()])
    }
    async function run(operation: () => Promise<AssetResidencyStatus | void>) {
        if (busy) return
        operating = true; failure = ''
        assetCleanupProgress.set(undefined); cleanupDetailsOpen = false
        // A failure the operation already explained stays over a later refresh error.
        try { const result = await operation(); if (mounted) await refresh(result) } catch (error) { failure ||= message(error) } finally { operating = false }
    }
    async function cleanCopies() {
        try { return await evictLocalAssets() }
        catch (error) {
            if ($assetCleanupProgress?.state === 'cancelled') return
            failure ||= message(error)
            if (mounted) await readResidency().catch(() => {})
            throw error
        }
    }
    // A new code replaces the refusal of the previous one. Other connection failures stay.
    function replaceCandidate(next: ServerConfig) {
        candidate = next; failure = ''
        if (usedRegistrationCodes.includes(view.error)) dismissServerSyncConnectionFailure()
    }
    function readCode() { try { replaceCandidate(parseServerRegistration(code)); code = '' } catch { failure = copy.registrationInvalid } }
    async function scan() { scanning = true; try { replaceCandidate(await scanner.scan(tone)) } catch (error) { if (!isQrScanCancelled(error)) failure = message(error) } finally { scanning = false } }
    // A device that is not connected chooses its asset storage with the connection; a connected one keeps its group.
    async function connect(newDevice = false) {
        if (!candidate) return
        const choice = connected ? undefined : policy
        const kept = storedPolicy
        try { await connectTarget(candidate, newDevice, choice) }
        catch (error) {
            // A refused code is put away so a new one can be entered.
            if (usedRegistrationCodes.includes(errorCode(error))) candidate = undefined
            throw error
        }
        candidate = undefined; codeOpen = false
        // Connecting applies only a remote choice, so a device that kept assets on the server and chose full downloads them here.
        if (choice === 'full' && kept === 'remote' && connected) return downloadHeld()
    }
    async function downloadAll() { downloading = true; try { return await controller.track('assets', () => setAssetResidencyPolicy('full')) } finally { downloading = false } }
    async function downloadHeld() { const release = await holdServerSync(); try { return await downloadAll() } finally { await release() } }
    async function disconnect() {
        let serverObjects: number | undefined
        let timer: ReturnType<typeof setTimeout> | undefined
        checkingFiles = true
        try {
            serverObjects = await Promise.race([
                getAssetResidencyStatus().then(status => status.serverObjects),
                new Promise<undefined>(resolve => { timer = setTimeout(() => resolve(undefined), SERVER_ONLY_CHECK_MS) }),
            ])
        } catch {} finally {
            clearTimeout(timer)
            checkingFiles = false
        }
        if (serverObjects === 0) {
            if (await alertActionConfirm({ title: copy.disconnectTitle, description: copy.disconnectDescription, actionLabel: copy.disconnect, cancelLabel: language.cancel })) await disconnectServerSync()
            return
        }
        const choice = await alertCheckboxConfirm({ title: copy.disconnectTitle, description: serverObjects === undefined ? copy.disconnectRemoteOnlyUnknown : copy.disconnectRemoteOnly, checkboxLabel: copy.downloadThenDisconnect, actionLabel: copy.disconnect, cancelLabel: language.cancel, requireChecked: false })
        if (!choice.confirmed) return
        if (!choice.checked) return disconnectServerSync()
        const release = await holdServerSync()
        try {
            try { await downloadAll() } catch (error) {
                if (errorCode(error) === 'cancelled') return
                // Files that are on neither this device nor the server fail the download but are not lost by disconnecting.
                let remaining: number | undefined
                try { remaining = (await getAssetResidencyStatus()).serverObjects } catch {}
                if (remaining !== 0) { failure = copy.downloadFailedKeptConnection; return }
            }
            await disconnectServerSync()
        } finally { await release() }
    }
    async function switchFormat() {
        const lww = language.lwwSync
        const choice = await alertCheckboxConfirm({ title: lww.formatSwitchTitle, description: lww.formatSwitchDescription, checkboxLabel: lww.formatSwitchAcknowledge, actionLabel: lww.formatSwitch, cancelLabel: language.cancel, requireChecked: true })
        if (!choice.confirmed) return
        await run(async () => {
            // A library already above this app's level, or a server without levels, stops sync with its own guidance.
            try { await switchServerSyncLibraryFormat() } catch (error) {
                if (!['app-update-required', 'sync-server-update-required'].includes(errorCode(error))) { failure = lww.formatSwitchFailed; throw error }
            }
            await retryServerSync()
        })
    }
    async function cleanCache() { if (await alertConfirm(copy.management.cleanConfirm)) await cleanupServerSyncCache() }
    // A refused change puts the control back on the stored policy.
    function choosePolicy(next: AssetResidencyPolicy) { void run(() => next === 'full' ? downloadAll() : setAssetResidencyPolicy(next)).finally(() => { if (storedPolicy) policy = storedPolicy }) }
    $effect(() => { if (storedPolicy) policy = storedPolicy })
    $effect(() => {
        if (!view.progress) return
        now = Date.now()
        const timer = setInterval(() => { now = Date.now() }, 500)
        return () => clearInterval(timer)
    })
    $effect(() => {
        if (!pendingKey.startsWith('true:')) return
        void controller.pendingChanges().then(value => { pending = value }, () => {})
    })
    onMount(() => {
        if (!isTauri) return
        mounted = true
        let wasCleaning = cleaning
        stopCleanup = assetCleanupProgress.subscribe(value => {
            const active = isAssetCleanupRunning(value)
            if (wasCleaning && !active && !operating) void readResidency().catch(error => { failure ||= message(error) })
            wasCleaning = active
        })
        dispose = controller.subscribe(value => { view = value })
        stopWatching = controller.watchProgress()
        inboxDispose = serverRegistrationInbox.changed.subscribe(() => { const pending = serverRegistrationInbox.take(); if (pending) replaceCandidate(pending) })
        void refresh().catch(error => { failure ||= message(error) })
    })
    onDestroy(() => { mounted = false; stopCleanup(); dispose(); stopWatching(); inboxDispose(); scanner.cancel(); serverRegistrationInbox.releaseConsumed() })
</script>

{#snippet statusPill()}
    <StatusBadge label={status.label} tone={status.tone} />
{/snippet}

{#snippet connection()}
    {#if !connected}
        <div class="sync-block">
            <SettingNotice text={language.lwwSync.concurrentEditNotice} />
        </div>
    {/if}
    {#if view.bindingIncomplete}
        <div class="sync-block">
            <p role="status" class="text-sm">{language.lwwSync.bindingIncomplete}</p>
            <div class="actions">
                <SettingButton onclick={() => void run(completeServerSyncBinding)} busy={busy}>{copy.connect}</SettingButton>
            </div>
            {#if progressPlace === 'incomplete'}{@render progressPanel()}{/if}
        </div>
    {/if}
    {#if view.status.bound}
        <div class="sync-block">
            {#if connected && (view.status.libraryId || view.status.deviceId || pending !== undefined)}
                <dl class="kv">
                    {#if view.status.libraryId}<dt>{copy.libraryId}</dt><dd class="mono">{view.status.libraryId}</dd>{/if}
                    {#if view.status.deviceId}<dt>{copy.deviceId}</dt><dd class="mono">{view.status.deviceId}</dd>{/if}
                    {#if pending !== undefined}<dt>{copy.pendingChanges}</dt><dd>{copy.count.replace('{0}', pending.toLocaleString())}</dd>{/if}
                    {#if view.lastSuccessAt !== undefined && !progress}<dt>{copy.lastSuccess}</dt><dd>{new Date(view.lastSuccessAt).toLocaleString()}</dd>{/if}
                </dl>
            {/if}
            {#if progressPlace === 'bound'}{@render progressPanel()}{/if}
            <div class="actions">
                {#if view.paused && view.error === 'library-format-switch-required'}<SettingButton onclick={() => void switchFormat()} disabled={busy}>{language.lwwSync.formatSwitch}</SettingButton>{/if}
                {#if !view.bindingIncomplete}<SettingButton onclick={() => void run(retryServerSync)} disabled={busy}>{copy.syncNow}</SettingButton>{/if}
                <SettingButton variant="danger" class="ml-auto" onclick={() => void run(disconnect)} busy={checkingFiles} disabled={busy}>{copy.disconnect}</SettingButton>
            </div>
            {@render alert()}
        </div>
    {/if}
    {#if codeShown}
        <div class="sync-block">
            {#if candidate}
                <p class="block-title">{copy.reviewTitle}</p>
                <dl class="kv review">
                    <dt>{copy.endpoint}</dt><dd>{candidate.endpoint}</dd>
                    <dt>{copy.libraryId}</dt><dd class="mono">{candidate.libraryId}</dd>
                </dl>
                {#if !connected}
                    <div class="field-head">
                        <p class="block-title">{copy.residency.title}</p>
                        <p class="help">{copy.residency.description}</p>
                    </div>
                    {@render residencyChoices()}
                {/if}
                <div class="actions">
                    <SettingButton onclick={() => void run(() => connect())} busy={busy} disabled={busy}>{copy.connect}</SettingButton>
                    {#if writerRecovery}<SettingButton variant="secondary" onclick={() => void run(() => connect(true))} disabled={busy}>{language.lwwSync.newDeviceAction}</SettingButton>{/if}
                    <SettingButton variant="secondary" onclick={() => { candidate = undefined }} disabled={busy}>{copy.discardRegistration}</SettingButton>
                </div>
                {#if progressPlace === 'candidate'}{@render progressPanel()}{:else}<p class="help">{copy.connectHint}</p>{/if}
            {:else}
                <div class="field-head">
                    <label class="block-title" for="server-registration">{copy.registrationCode}</label>
                    {#if tone === 'settings'}<p class="help">{canScanServerRegistration ? copy.connectRowHelpScan : copy.connectRowHelp}</p>{/if}
                </div>
                <textarea id="server-registration" class="code" rows="3" spellcheck="false" autocapitalize="none" bind:value={code} disabled={busy || scanning}></textarea>
                <div class="actions">
                    <SettingButton onclick={readCode} disabled={busy || !code}>{copy.readRegistration}</SettingButton>
                    {#if canScanServerRegistration}
                        <SettingButton variant="secondary" onclick={() => void scan()} busy={scanning} disabled={busy}>{copy.scanRegistration}</SettingButton>
                    {/if}
                </div>
            {/if}
            {#if !view.status.bound}{@render alert()}{/if}
        </div>
    {:else}
        <SettingRow label={copy.registrationCode} help={canScanServerRegistration ? copy.connectRowHelpScan : copy.connectRowHelp}>
            <SettingButton variant="secondary" aria-expanded={false} onclick={() => { codeOpen = true }}>{copy.enterCode}</SettingButton>
        </SettingRow>
    {/if}
{/snippet}

{#snippet alert()}
    <!-- An error that is still being retried reads as information, not a failure to act on. -->
    {#if view.error || failure}{@const retrying = !failure && status.tone === 'failed'}<SettingNotice role={retrying ? 'status' : 'alert'} tone={retrying ? 'info' : 'danger'} text={failure || message({ code: view.error })} />{/if}
{/snippet}

{#snippet residencyChoices(apply?: (next: AssetResidencyPolicy) => void)}
    <div class="choices" role="radiogroup" aria-label={copy.residency.title} aria-disabled={busy ? 'true' : undefined}>
        {#each residencyOptions as option (option.value)}
            {@const checked = policy === option.value}
            <label class="choice" data-checked={checked} data-disabled={busy}>
                <input class="sr-only" type="radio" name="risunest-asset-residency" value={option.value} {checked} disabled={busy} onchange={() => { policy = option.value; apply?.(option.value) }} />
                <span class="choice-icon" aria-hidden="true">
                    {#if option.value === 'full'}<HardDriveIcon size={18} />{:else}<CloudDownloadIcon size={18} />{/if}
                </span>
                <span class="choice-label">{option.label}</span>
                <span class="choice-radio" aria-hidden="true"></span>
            </label>
        {/each}
    </div>
{/snippet}

{#snippet progressPanel()}
    {#if progress}
        <div class="progress" data-sync-progress data-mode={summary ? 'routine' : undefined}>
            <TransferProgress
                label={summary?.label ?? progress.label}
                detail={summary ? '' : progress.detail}
                fraction={summary ? summary.fraction : progress.fraction}
                done={summary?.complete}
                activity={summary && !summary.complete ? (progress.detail ? `${progress.label} · ${progress.detail}` : progress.label) : ''}
                stages={progress.stages} counters={progress.counters} detailsLabel={copy.details}
                collapsible={!!summary} bind:open={detailsOpen}
                speed={view.progress?.network ? {
                    sample: view.progress.network,
                    active: view.progress.pausedAt === undefined && !view.paused && view.progress.active.some(stage => stage !== 'applying' && stage !== 'refreshing'),
                    uploadLabel: copy.uploadSpeed, downloadLabel: copy.downloadSpeed,
                } : undefined} />
        </div>
    {/if}
{/snippet}
{#snippet place(part: 'local' | 'server' | 'external' | 'missing', label: string, value?: string)}
    <div class="place" data-part={part}>
        <span class="place-label"><i class="place-dot" aria-hidden="true"></i><span>{label}</span></span>
        {#if value === undefined}<span class="value-pending motion-safe:animate-pulse"></span>{:else}<span class="value">{value}</span>{/if}
    </div>
{/snippet}

{#if isTauri}
    {#if tone === 'onboarding'}
        <div class="embedded">
            {#if status.tone !== 'idle'}<div class="embedded-status">{@render statusPill()}</div>{/if}
            {@render connection()}
        </div>
    {:else}
        <SettingGroup id="risunest-server-sync" title={copy.title} description={copy.description} actions={statusPill}>
            {@render connection()}
        </SettingGroup>
    {/if}
    {#if connected && storedPolicy}
        <SettingGroup title={copy.residency.title} description={copy.residency.description}>
            <div class="sync-block">
                {@render residencyChoices(choosePolicy)}
            </div>
            {#if residency}
                {@const externalBytes = residency.remoteObjects > residency.serverObjects ? residency.remoteBytes - residency.serverBytes : 0}
                {@const shares = [
                    { part: 'local', bytes: residency.localBytes },
                    { part: 'server', bytes: residency.serverBytes },
                    { part: 'external', bytes: externalBytes },
                ].filter(share => share.bytes > 0)}
                <div class="sync-block">
                    {#if shares.length > 0}
                        <div class="distribution" aria-hidden="true">
                            {#each shares as share (share.part)}<span data-part={share.part} style:flex-grow={share.bytes}></span>{/each}
                        </div>
                    {/if}
                    <div class="places">
                        {@render place('local', copy.residency.local, bytes(residency.localBytes))}
                        {@render place('server', copy.residency.remoteOnly, bytes(residency.serverBytes))}
                        {#if residency.remoteObjects > residency.serverObjects}{@render place('external', copy.residency.externalOnly, bytes(externalBytes))}{/if}
                        {@render place('missing', copy.residency.unavailable, copy.count.replace('{0}', residency.unavailableObjects.toLocaleString()))}
                    </div>
                </div>
            {:else if !residencyFailed}
                <!-- The placeholder keeps the loaded shape: the status line stands where the bar goes, above the same rows. -->
                <div class="sync-block" data-residency-loading role="status" aria-live="polite">
                    <p class="checking"><LoaderCircleIcon size={14} class="shrink-0 motion-safe:animate-spin" aria-hidden="true" /><span>{copy.residency.checking}</span></p>
                    <div class="places" aria-hidden="true">
                        {@render place('local', copy.residency.local)}
                        {@render place('server', copy.residency.remoteOnly)}
                        {@render place('missing', copy.residency.unavailable)}
                    </div>
                </div>
            {/if}
            <div class="sync-block">
                <div class="actions">
                    {#if residency?.policy === 'full' && residency.remoteObjects > 0}<SettingButton onclick={() => void run(downloadHeld)} busy={downloading} disabled={busy}>{copy.residency.download}</SettingButton>{/if}
                    <SettingButton variant="secondary" onclick={() => void run(cleanCopies)} disabled={busy}>{copy.residency.clean}</SettingButton>
                    {#if cleaning || (operating && !$assetCleanupProgress)}<SettingButton variant="secondary" disabled={$assetCleanupProgress?.state === 'cancelling'} onclick={() => cleaning ? cancelLocalAssetCleanup() : void cancelAssetResidencyOperation()}>{copy.residency.cancel}</SettingButton>{/if}
                </div>
                {#if $assetCleanupProgress}
                    {@const cleanup = $assetCleanupProgress}
                    {@const terminal = !isAssetCleanupRunning(cleanup)}
                    {@const measured = cleanup.state === 'running' && cleanup.totalItems !== null && cleanup.totalItems > 0}
                    <div data-asset-cleanup-progress>
                        <TransferProgress
                            label={copy.residency.cleanup[cleanup.state === 'running' ? cleanup.stage : cleanup.state]}
                            detail={cleanup.state === 'complete' ? copy.count.replace('{0}', cleanup.evictedItems.toLocaleString()) : measured ? `${cleanup.completedItems.toLocaleString()} / ${cleanup.totalItems!.toLocaleString()}` : ''}
                            fraction={cleanup.state === 'complete' ? 1 : measured ? cleanup.completedItems / cleanup.totalItems! : null}
                            done={cleanup.state === 'complete'} stopped={terminal && cleanup.state !== 'complete'}
                            counters={cleanup.evictedBytes > 0 || cleanup.state === 'complete' ? [{ key: 'freed', label: copy.residency.cleanup.freed, value: bytes(cleanup.evictedBytes) }] : []}
                            detailsLabel={copy.details} collapsible bind:open={cleanupDetailsOpen}
                        />
                    </div>
                {/if}
                {#if progressPlace === 'assets'}{@render progressPanel()}{/if}
                {#if downloading}<p role="status" class="text-sm">{copy.residency.working}</p>{/if}
                <p class="help">{copy.residency.cleanupNote}</p>
            </div>
        </SettingGroup>
    {/if}
    {#if cache && tone === 'settings'}
        <SettingGroup title={copy.management.title}>
            <SettingRow inline label={copy.management.cache}><span class="value">{bytes(cache.cacheBytes)}</span></SettingRow>
            <SettingRow inline label={copy.management.protected}><span class="value">{bytes(cache.protectedBytes)}</span></SettingRow>
            <SettingRow inline label={copy.management.ledger} help={copy.management.ledgerHelp}><span class="value">{bytes(cache.ledgerBytes)}</span></SettingRow>
            <div class="sync-block">
                <div class="actions">
                    <SettingButton variant="secondary" onclick={() => void run(cleanCache)} disabled={busy || !!cache.blockedReason}>{copy.management.clean}</SettingButton>
                </div>
                {#if cache.blockedReason}<p class="help">{describeBlockedReason(cache.blockedReason)}</p>{/if}
            </div>
        </SettingGroup>
    {/if}
{/if}

<style>
    .sync-block {
        display: grid;
        gap: 0.75rem;
        min-width: 0;
        padding: 0.875rem 1rem;
        overflow-wrap: anywhere;
    }
    .actions {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 0.5rem;
    }
    .help {
        max-width: 62ch;
        font-size: 12.5px;
        line-height: 1.55;
        color: var(--color-subtext);
    }
    .block-title {
        font-size: 14px;
        font-weight: 600;
    }
    .field-head {
        display: grid;
        gap: 0.25rem;
    }
    .mono {
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
        font-size: 12.5px;
    }
    .code {
        width: 100%;
        min-height: 4.5rem;
        resize: vertical;
        padding: 0.5rem 0.75rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.375rem;
        background: var(--risu-theme-bgcolor);
        color: var(--risu-theme-textcolor);
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
        font-size: 12.5px;
        line-height: 1.5;
        word-break: break-all;
    }
    .code:focus-visible {
        outline: 2px solid var(--risu-theme-selected);
        outline-offset: 1px;
    }
    .code:disabled {
        opacity: 0.5;
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
    .kv.review {
        padding: 0.75rem 0.875rem;
        /* Android WebView 113 computes a color-mix() border shorthand to currentColor. */
        border-width: 1px;
        border-style: solid;
        border-color: color-mix(in srgb, var(--risu-theme-primary-500) 35%, transparent);
        border-radius: 0.5rem;
        background: color-mix(in srgb, var(--risu-theme-primary-500) 8%, transparent);
    }
    .kv.review dd {
        font-weight: 600;
    }
    .progress {
        display: grid;
        gap: 0.625rem;
        min-width: 0;
    }
    /* An automatic sync bar fills from empty when it appears. */
    .progress[data-mode='routine'] :global([role='progressbar'] > div) {
        animation: risunest-sync-fill 0.6s ease-out;
    }
    @keyframes -global-risunest-sync-fill {
        from {
            width: 0;
        }
    }
    .value {
        font-size: 14px;
        font-variant-numeric: tabular-nums;
        white-space: nowrap;
    }
    .choices {
        display: grid;
        gap: 0.5rem;
    }
    @container (min-width: 36rem) {
        .choices {
            grid-template-columns: repeat(2, minmax(0, 1fr));
        }
    }
    .choice {
        display: flex;
        align-items: center;
        gap: 0.75rem;
        min-width: 0;
        padding: 0.75rem 0.875rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
        cursor: pointer;
        transition: border-color 0.2s, background-color 0.2s;
    }
    .choice:hover {
        border-color: color-mix(in srgb, var(--risu-theme-textcolor2) 55%, var(--risu-theme-darkborderc));
    }
    .choice[data-checked='true'] {
        border-color: var(--risu-theme-primary-500);
        background: color-mix(in srgb, var(--risu-theme-primary-500) 9%, var(--risu-theme-bgcolor));
    }
    .choice[data-disabled='true'] {
        cursor: not-allowed;
        opacity: 0.6;
    }
    .choice:has(input:focus-visible) {
        outline: 2px solid var(--risu-theme-selected);
        outline-offset: 2px;
    }
    .choice-icon {
        display: grid;
        flex: none;
        place-items: center;
        width: 2.25rem;
        height: 2.25rem;
        border-radius: 0.5rem;
        background: var(--risu-theme-darkbg);
        color: var(--color-subtext);
    }
    .choice[data-checked='true'] .choice-icon {
        background: color-mix(in srgb, var(--risu-theme-primary-500) 18%, var(--risu-theme-bgcolor));
        color: var(--risu-theme-textcolor);
    }
    .choice-label {
        flex: 1;
        min-width: 0;
        font-size: 14px;
        line-height: 1.4;
    }
    .choice[data-checked='true'] .choice-label {
        font-weight: 600;
    }
    .choice-radio {
        flex: none;
        width: 1.125rem;
        height: 1.125rem;
        border: 2px solid var(--risu-theme-darkborderc);
        border-radius: 50%;
    }
    .choice[data-checked='true'] .choice-radio {
        border-color: var(--risu-theme-primary-500);
        box-shadow: inset 0 0 0 3px var(--risu-theme-bgcolor);
        background: var(--risu-theme-primary-500);
    }
    .distribution {
        display: flex;
        gap: 2px;
        height: 0.5rem;
        overflow: hidden;
        border-radius: 99px;
        background: var(--risu-theme-darkbutton);
    }
    .distribution span {
        flex-basis: 0;
        min-width: 3px;
    }
    .places {
        display: grid;
        gap: 0.5rem;
    }
    .checking {
        display: flex;
        align-items: center;
        gap: 0.5rem;
        min-width: 0;
        font-size: 12.5px;
        line-height: 1rem;
        color: var(--color-subtext);
    }
    .value-pending {
        flex: none;
        align-self: center;
        width: 3.5rem;
        height: 0.875rem;
        border-radius: 0.25rem;
        background: var(--risu-theme-darkbutton);
    }
    .place {
        display: flex;
        align-items: baseline;
        justify-content: space-between;
        gap: 1rem;
        min-width: 0;
        font-size: 13.5px;
    }
    .place-label {
        display: inline-flex;
        align-items: center;
        gap: 0.5rem;
        min-width: 0;
        color: color-mix(in srgb, var(--risu-theme-textcolor2) 45%, var(--risu-theme-textcolor));
    }
    .place-dot {
        flex: none;
        width: 0.625rem;
        height: 0.625rem;
        border-radius: 3px;
    }
    .distribution [data-part='local'],
    .place[data-part='local'] .place-dot {
        background: var(--risu-theme-primary-500);
    }
    .distribution [data-part='server'],
    .place[data-part='server'] .place-dot {
        background: var(--risu-theme-primary-300);
    }
    .distribution [data-part='external'],
    .place[data-part='external'] .place-dot {
        background: var(--risu-theme-success-500);
    }
    .place[data-part='missing'] .place-dot {
        border: 1.5px dashed var(--risu-theme-textcolor2);
    }
    .embedded {
        display: grid;
        gap: 0.25rem;
        min-width: 0;
    }
    .embedded-status {
        display: flex;
        padding: 0 0 0.25rem;
    }
    .embedded .sync-block {
        padding: 0.5rem 0;
    }
    @media (prefers-reduced-motion: reduce) {
        .choice,
        .progress[data-mode='routine'] :global([role='progressbar'] > div) {
            animation: none;
            transition: none;
        }
    }
</style>
