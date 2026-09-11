<script lang="ts">
    import { onMount } from 'svelte'
    import { ArrowDownLeft, ArrowLeftRight, ArrowUpRight, Download, Monitor, RefreshCw, Smartphone } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertToast } from 'src/ts/alert'
    import { isTauriAndroid } from 'src/ts/platform'
    import { getDatabase } from 'src/ts/storage/database.svelte'
    import { getDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'
    import { formatRisuNestStorageBytes } from 'src/ts/storage/risuNestStorageDashboard'
    import { decodeLogicalRecordKey } from 'src/ts/storage/sync/logicalRecordKey'
    import { getProductionDeviceSyncController } from 'src/ts/storage/sync/deviceSyncProduction'
    import type { DeviceSyncControllerSnapshot } from 'src/ts/storage/sync/deviceSyncController'
    import { classifyDeviceSyncFailure, parseDeviceSyncUri } from 'src/ts/storage/sync/deviceSync'
    import type { DeviceSyncErrorCode } from 'src/ts/storage/sync/deviceSync'
    import { androidPeerSyncNotificationsEnabled } from 'src/ts/storage/sync/peerSyncShared'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SegmentedButtons from '../RisuNest/SegmentedButtons.svelte'

    type WorkLane = 'clone' | 'delta' | 'bidirectional'
    type ListenMethod = ReturnType<typeof getDeviceSettings>['syncListenMethod']
    interface PeerEntry {
        deviceId: string
        name?: string | null
        lastSeenMs?: number
        totalBytes?: number
        permissions: readonly string[]
    }

    const controller = getProductionDeviceSyncController()
    const sync = language.risuNest.sync
    // Registration links expire ten minutes after they are created.
    const linkLifetimeMs = 10 * 60 * 1000
    const initialSnapshot = controller.snapshot()
    let snapshot = $state<DeviceSyncControllerSnapshot>(initialSnapshot)
    let settings = $state(getDeviceSettings())
    let permissions = $state({ read: true, bidirectional: false })
    let targetId = $state('new-link')
    let stagedUri = $state(initialSnapshot.stagedUri ?? '')
    let observedStagedUri = initialSnapshot.stagedUri
    let qrDataUrl = $state('')
    let qrUri = ''
    let copyFailed = $state(false)
    let now = $state(Date.now())
    let notificationsEnabled = $state<boolean | null>(null)
    let sourceTransition = $state(false)
    let activeWork = $state<WorkLane | null>(null)
    let suppressWorkInference = false
    let workActionPending = $state(false)
    // Held across a confirmation dialog so a second click cannot open a second
    // dialog, without claiming the operation itself is already running.
    let confirmPending = $state(false)
    // A terminal desktop clone stays resumable forever, so the reader needs a
    // way to say they are done with it or every other task stays locked out.
    let cloneDismissed = $state(false)
    let shareActionError = $state<string | null>(null)
    let workActionError = $state<string | null>(null)

    const sourceBusy = $derived(['preparing', 'prepared', 'starting', 'running', 'stopping'].includes(snapshot.source.phase))
    const sourceError = $derived(snapshot.source.latestError ?? snapshot.sourceError)
    const pairUri = $derived(snapshot.source.pairingUri ?? '')
    const expired = $derived(!snapshot.source.expiresAtMs || snapshot.source.expiresAtMs <= now)
    const remaining = $derived(snapshot.source.expiresAtMs ? Math.max(0, snapshot.source.expiresAtMs - now) : 0)
    const remainingRatio = $derived(Math.max(0, Math.min(1, remaining / linkLifetimeMs)))
    const clone = $derived(snapshot.targets.clone)
    const delta = $derived(snapshot.targets.delta)
    const bidi = $derived(snapshot.targets.bidirectional)
    const cloneTarget = $derived(clone?.state.target)
    const bidiPhase = $derived(bidi?.operationPhase ?? 'idle')
    const selectedIncoming = $derived(snapshot.sources.find((source) => source.deviceId === targetId))
    const selectedExpired = $derived(targetId !== 'new-link' && snapshot.expiredSourceIds.includes(targetId))
    const stagedLinkInvalid = $derived(
        targetId === 'new-link' && stagedUri.trim().length > 0 && !parsesAsLink(stagedUri),
    )
    // A link that grants nothing registers a device that can never do anything,
    // so it is refused here rather than after the other device scans it.
    const permissionsChosen = $derived(permissions.read || permissions.bidirectional)
    // A recovered conflict has no in-memory source, so the reader has to name
    // the device before either winner can be sent.
    const conflictSourceId = $derived(
        snapshot.activeBidirectionalSourceDeviceId ?? snapshot.stagedSourceDeviceId ?? selectedIncoming?.deviceId ?? null,
    )
    const cloneTerminal = $derived(cloneTarget?.phase === 'cancelled' || cloneTarget?.phase === 'failed')
    const cloneRetryable = $derived(Boolean(clone?.resumeAvailable) || (
        !isTauriAndroid && cloneTerminal && !cloneDismissed
    ))
    const cloneBusy = $derived(
        workActionPending && activeWork === 'clone'
        || cloneTarget?.phase === 'downloading'
        || cloneTarget?.phase === 'confirmed'
        || cloneRetryable
    )
    const deltaRetained = $derived(delta?.retained ?? null)
    // Continuing runs the same registered pull, so it needs everything that
    // pull needs: a resumable witness and a source that is still registered,
    // still readable and not known to have expired.
    const deltaResumable = $derived(Boolean(
        deltaRetained
        && deltaRetained.witness !== 'ambiguous'
        && !snapshot.expiredSourceIds.includes(deltaRetained.sourceDeviceId)
        && snapshot.sources.some((source) => (
            source.deviceId === deltaRetained.sourceDeviceId && source.permissions.includes('read')
        ))
    ))
    const deltaBusy = $derived(
        workActionPending && activeWork === 'delta'
        || delta?.pullPhase === 'running'
        || Boolean(deltaRetained)
    )
    const bidiBusy = $derived(
        workActionPending && activeWork === 'bidirectional'
        || ['running', 'awaitingConflict', 'sourcePrepared', 'targetPrepared', 'localCommitted', 'sourceUnavailable', 'refreshPending'].includes(bidiPhase)
    )
    const receiveWorkBusy = $derived(cloneBusy || deltaBusy || bidiBusy)
    const receiveBusy = $derived(sourceBusy || receiveWorkBusy)
    const latestWorkError = $derived(
        workActionError
        ?? (snapshot.workError ? safeError(snapshot.workError) : null)
        ?? (activeWork === 'clone' && clone?.error ? safeError(clone.error) : null)
        ?? (activeWork === 'delta' && delta?.error ? safeError(delta.error) : null)
        ?? (activeWork === 'bidirectional' && bidi?.operationError ? safeError(bidi.operationError) : null)
    )
    const running = $derived(snapshot.source.phase === 'running')
    const methodOptions: { value: ListenMethod; label: string }[] = [
        { value: 'lan', label: sync.share.methodLan },
        { value: 'quick', label: sync.share.methodQuick },
        { value: 'fixed-url', label: sync.share.methodFixed },
    ]
    const methodHelp = $derived(
        settings.syncListenMethod === 'quick' ? sync.share.quickNote
            : settings.syncListenMethod === 'fixed-url' ? sync.share.fixedNote
            : sync.share.methodLanHelp,
    )
    const laneTitle = $derived(
        activeWork === 'clone' ? sync.work.clone
            : activeWork === 'delta' ? sync.work.delta
            : sync.work.bidirectional,
    )
    const inputClass = 'rounded-md border border-darkborderc bg-bgcolor px-3 py-1.5 text-sm text-textcolor transition-colors duration-200 focus:border-borderc focus:ring-2 focus:ring-borderc/45 focus:outline-hidden'
    const revokeClass = 'shrink-0 rounded-md border border-darkborderc bg-transparent px-2 py-1 text-sm text-textcolor2 transition-colors duration-200 hover:border-draculared hover:bg-draculared/10 hover:text-draculared focus:outline-hidden focus:ring-2 focus:ring-draculared disabled:cursor-not-allowed disabled:opacity-50'
    const tileClass = 'flex flex-col gap-1 rounded-lg border border-darkborderc bg-darkbutton px-3.5 py-3 text-left shadow-xs transition-colors duration-200 hover:bg-selected focus:outline-hidden focus:ring-2 focus:ring-selected disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-darkbutton'

    function inferWork(value: DeviceSyncControllerSnapshot): WorkLane | null {
        const bidiValue = value.targets.bidirectional
        const cloneValue = value.targets.clone
        const bidiActive = bidiValue && (bidiValue.operationRetained || [
            'running', 'awaitingConflict', 'sourcePrepared', 'targetPrepared', 'localCommitted', 'sourceUnavailable', 'refreshPending',
        ].includes(bidiValue.operationPhase))
        const clonePhase = cloneValue?.state.target.phase
        const cloneActive = cloneValue && (cloneValue.resumeAvailable || ['joined', 'confirmed', 'downloading', 'cancelled'].includes(clonePhase ?? 'idle'))
        if (bidiActive) return 'bidirectional'
        if (cloneActive) return 'clone'
        if (value.targets.delta?.retained) return 'delta'
        if (value.targets.delta?.pullPhase === 'running') return 'delta'
        if (value.activeBidirectionalSourceDeviceId && bidiValue?.operationPhase !== 'idle') return 'bidirectional'
        if (value.activeCloneSourceDeviceId && clonePhase !== 'idle') return 'clone'
        if (value.targets.delta?.pullPhase !== undefined && value.targets.delta.pullPhase !== 'idle') return 'delta'
        if (bidiValue?.operationPhase !== 'idle') return 'bidirectional'
        if (clonePhase !== undefined && clonePhase !== 'idle') return 'clone'
        return null
    }

    function stateLabel(): string {
        if (sourceError || snapshot.source.phase === 'error') return sync.share.stateError
        if (snapshot.source.phase === 'running') return sync.share.stateListening
        if (snapshot.source.phase === 'stopping') return sync.share.stateStopping
        if (['prepared', 'preparing', 'starting'].includes(snapshot.source.phase)) return sync.share.statePreparing
        return sync.share.stateOff
    }
    function statePill(): { border: string; dot: string } {
        if (sourceError || snapshot.source.phase === 'error') return { border: 'border-draculared bg-draculared/10 text-textcolor', dot: 'bg-draculared' }
        if (snapshot.source.phase === 'running') return { border: 'border-success-500 bg-success-500/10 text-textcolor', dot: 'bg-success-500' }
        if (['prepared', 'preparing', 'starting', 'stopping'].includes(snapshot.source.phase)) return { border: 'border-darkborderc bg-darkbutton text-textcolor', dot: 'bg-borderc' }
        return { border: 'border-darkborderc text-textcolor2', dot: 'bg-textcolor2' }
    }
    function deviceIcon(name: string | null | undefined): typeof Monitor {
        return name === 'Android' ? Smartphone : Monitor
    }
    function format(template: string, ...values: Array<string | number>): string {
        return values.reduce<string>((value, next, index) => value.replace(`{${index}}`, String(next)), template)
    }
    function updateSettings(partial: Partial<typeof settings>): void {
        updateDeviceSettings(partial)
        settings = getDeviceSettings()
    }
    function formatTime(value?: number): string {
        return value ? new Date(value).toLocaleString() : '-'
    }
    function formatDuration(milliseconds: number): string {
        const totalSeconds = Math.max(0, Math.ceil(milliseconds / 1000))
        const minutes = Math.floor(totalSeconds / 60)
        const seconds = totalSeconds % 60
        const minuteText = format(minutes === 1 ? sync.share.durationMinute : sync.share.durationMinutes, minutes)
        const secondText = format(seconds === 1 ? sync.share.durationSecond : sync.share.durationSeconds, seconds)
        return minutes > 0 ? `${minuteText} ${secondText}` : secondText
    }
    function parsesAsLink(value: string): boolean {
        try {
            parseDeviceSyncUri(value)
            return true
        } catch {
            return false
        }
    }
    function safeError(code: unknown): string {
        const safeCode = classifyDeviceSyncFailure(code).code
        // The controller stores its own classified code, so both shapes reach here.
        const is = (value: DeviceSyncErrorCode) => code === value || safeCode === value
        if (is('registration-expired') || is('transport-changed')) return sync.registrationExpired
        if (is('registration-blocked-by-active-work')) return sync.work.registerBlockedByActiveWork
        if (is('source-in-use')) return sync.work.sourceInUse
        if (is('source-changed')) return sync.work.sourceChanged
        if (is('delta-completion-retained')) return sync.work.deltaBlockedByRetained
        if (is('peer-outdated')) return sync.work.peerOutdated
        if (safeCode === 'port-unavailable') return sync.share.errorPortUnavailable
        if (safeCode === 'lan-address-unavailable') return sync.share.errorLanAddressUnavailable
        if (safeCode === 'invalid-configuration') return sync.share.errorInvalidConfiguration
        if (safeCode === 'cleanup-failed') return sync.share.errorCleanupFailed
        if (safeCode === 'transport-unavailable') return sync.errorTransportUnavailable
        // Every remaining bounded code names an internal state the reader can do
        // nothing with, so they all share one readable next step.
        return sync.errorGeneric
    }
    function cloneBackupPaths(): string[] {
        const paths = cloneTarget?.backupPaths
        return Array.isArray(paths) ? paths.filter((path): path is string => typeof path === 'string' && path.length > 0) : []
    }
    function sourceRequest() {
        return {
            method: isTauriAndroid ? 'lan' as const : settings.syncListenMethod,
            fixedPort: settings.syncFixedPort,
            publicBaseUrl: settings.syncPublicBaseUrl,
        }
    }
    async function runAction(action: () => Promise<unknown>, scope: 'share' | 'work' = 'share'): Promise<boolean> {
        if (scope === 'work') workActionError = null
        else shareActionError = null
        try {
            await action()
            return true
        } catch (error) {
            if (scope === 'work') workActionError = safeError(error)
            else shareActionError = safeError(error)
            return false
        }
    }
    async function createQr(): Promise<void> {
        const uri = pairUri
        if (!uri || expired) { qrDataUrl = ''; qrUri = ''; return }
        if (qrDataUrl && qrUri === uri) return
        qrDataUrl = ''
        qrUri = ''
        const { default: QRCode } = await import('qrcode')
        const created = await QRCode.toDataURL(uri, { margin: 1, width: 192 })
        if (pairUri === uri && snapshot.source.expiresAtMs && snapshot.source.expiresAtMs > Date.now()) {
            qrDataUrl = created
            qrUri = uri
        }
    }
    // Every sharing-side command shares one native slot, so a second click has
    // to be refused here instead of surfacing as an unavailable-state failure.
    async function runShareAction(action: () => Promise<unknown>): Promise<boolean> {
        if (sourceTransition) return false
        sourceTransition = true
        try {
            return await runAction(action)
        } finally {
            sourceTransition = false
        }
    }
    async function confirmOnce(message: string): Promise<boolean> {
        if (confirmPending) return false
        confirmPending = true
        try {
            return await alertConfirm(message)
        } finally {
            confirmPending = false
        }
    }
    async function startOrStop(): Promise<void> {
        await runShareAction(async () => {
            if (snapshot.source.phase === 'running') { await controller.stop(); return }
            if (snapshot.source.phase !== 'prepared') await controller.prepare(sourceRequest())
            await controller.start(permissions)
        })
    }
    async function copyLink(): Promise<void> {
        if (!pairUri || expired) return
        try {
            await navigator.clipboard.writeText(pairUri)
            copyFailed = false
            alertToast(language.clipboardSuccess)
        } catch {
            // Without a clipboard the link has to become readable on screen, or
            // it has no way at all of reaching the other device.
            copyFailed = true
        }
    }
    async function rotate(): Promise<void> {
        copyFailed = false
        if (await runShareAction(() => controller.rotateLink(permissions))) await createQr()
    }
    async function revoke(direction: 'incoming' | 'outgoing', deviceId: string): Promise<void> {
        if (sourceTransition || !await confirmOnce(sync.devices.revokeConfirm)) return
        const removed = await runShareAction(() => direction === 'incoming' ? controller.revokeIncoming(deviceId) : controller.revokeOutgoing(deviceId))
        if (removed && direction === 'incoming' && targetId === deviceId) targetId = 'new-link'
    }
    async function beginWork(lane: WorkLane, action: () => Promise<unknown>): Promise<void> {
        if (receiveBusy || !workAllowed(lane)) return
        suppressWorkInference = false
        cloneDismissed = false
        activeWork = lane
        if (await runWorkAction(action)) {
            controller.clearStagedLink()
            stagedUri = ''
        }
    }
    function workAllowed(lane: WorkLane): boolean {
        if (selectedExpired) return false
        if (targetId === 'new-link') {
            if (!stagedUri.trim()) return Boolean(snapshot.stagedSourceDeviceId)
            return parsesAsLink(stagedUri)
        }
        return lane === 'bidirectional'
            ? Boolean(selectedIncoming?.permissions.includes('bidirectional'))
            : Boolean(selectedIncoming?.permissions.includes('read'))
    }
    async function runWorkAction(action: () => Promise<unknown>): Promise<boolean> {
        if (workActionPending) return false
        workActionPending = true
        const completed = await runAction(action, 'work')
        workActionPending = false
        return completed
    }
    function stageSelectedLink(): void {
        if (
            targetId === 'new-link'
            && stagedUri.trim().length > 0
            && stagedUri !== snapshot.stagedUri
        ) controller.stageLink(stagedUri)
    }
    function updateStagedUri(value: string): void {
        stagedUri = value
        if (!value.trim()) controller.clearStagedLink()
    }
    async function startClone(): Promise<void> {
        if (!await confirmOnce(sync.work.cloneConfirm)) return
        await beginWork('clone', async () => {
            stageSelectedLink()
            if (targetId === 'new-link') await controller.claimStagedClone()
            else await controller.selectRegisteredClone(targetId)
            await controller.confirmCloneReplace()
            await controller.downloadClone()
        })
    }
    async function startDelta(): Promise<void> {
        await beginWork('delta', async () => {
            stageSelectedLink()
            if (targetId === 'new-link') await controller.pullStagedDelta()
            else await controller.pullRegisteredDelta(targetId)
        })
    }
    async function startBidi(): Promise<void> {
        await beginWork('bidirectional', async () => {
            stageSelectedLink()
            if (targetId === 'new-link') await controller.syncStagedBidirectional()
            else await controller.syncRegisteredBidirectional(targetId)
        })
    }
    async function resolve(winner: 'local' | 'remote'): Promise<void> {
        const sourceId = conflictSourceId
        if (sourceId) await runWorkAction(() => controller.resolveRegisteredBidirectional(sourceId, winner))
    }
    async function resumeBidi(): Promise<void> {
        if (bidiPhase === 'sourcePrepared') {
            await runWorkAction(() => controller.rehostBidirectionalSource(sourceRequest(), permissions))
            return
        }
        await runWorkAction(() => controller.resumeBidirectional())
    }
    async function abandon(): Promise<void> {
        // The confirmation runs outside the pending flag so the panel does not
        // claim the sync is running while it asks whether to cancel it.
        if (workActionPending || !await confirmOnce(sync.work.abandonConfirm)) return
        if (await runWorkAction(() => controller.abandonBidirectional())) dismissWork()
    }
    async function resumeRetainedDelta(): Promise<void> {
        const sourceDeviceId = deltaRetained?.sourceDeviceId
        if (!sourceDeviceId) return
        await runWorkAction(() => controller.pullRegisteredDelta(sourceDeviceId))
    }
    async function abandonRetainedDelta(): Promise<void> {
        if (workActionPending || !await confirmOnce(sync.work.deltaAbandonConfirm)) return
        // A cancellation the target reports as still retained leaves the
        // panel up, so the only way out stays where the reader left it.
        if (await runWorkAction(() => controller.abandonDelta()) && !deltaRetained) dismissWork()
    }
    async function acknowledgeBidi(): Promise<void> {
        if (await runWorkAction(() => controller.acknowledgeBidirectional())) dismissWork()
    }
    function dismissWork(): void {
        suppressWorkInference = true
        activeWork = null
    }
    async function dismissClone(): Promise<void> {
        if (isTauriAndroid && cloneTarget?.phase === 'failed') {
            if (!await runWorkAction(() => controller.cancelClone())) return
        }
        cloneDismissed = true
        dismissWork()
    }
    function conflictNames(): { names: string[], otherCount: number } {
        const result = bidi?.operationResult
        if (!result || result.kind !== 'conflict') return { names: [], otherCount: 0 }
        const db = getDatabase()
        const names: string[] = []
        let otherCount = 0
        for (const conflict of result.conflicts) {
            try {
                const locator = decodeLogicalRecordKey(conflict.key)
                if (locator.kind === 'character' || locator.kind === 'conversation') {
                    const character = db.characters.find((entry) => entry.chaId === locator.characterId)
                    if (locator.kind === 'character' && character?.name) { names.push(character.name); continue }
                    if (locator.kind === 'conversation') {
                        const chat = character?.chats?.find((entry) => entry.id === locator.conversationId)
                        if (chat?.name) { names.push(chat.name); continue }
                    }
                }
            } catch {
                // Invalid and non-displayable keys are intentionally grouped below.
            }
            otherCount += 1
        }
        return { names: [...new Set(names)], otherCount }
    }

    function refreshNotificationState(): void {
        notificationsEnabled = androidPeerSyncNotificationsEnabled()
    }
    function openNotificationSettings(): void {
        try {
            window.RisuGenerationKeepAlive?.openNotificationSettings()
        } catch {
            // Android resumes this WebView with the real state either way.
        }
    }

    onMount(() => {
        activeWork ??= inferWork(snapshot)
        const unsubscribe = controller.subscribe((next) => {
            snapshot = next
            if (next.stagedUri !== observedStagedUri) {
                observedStagedUri = next.stagedUri
                stagedUri = next.stagedUri ?? ''
                if (next.stagedUri) targetId = 'new-link'
            }
            if (targetId !== 'new-link' && !next.sources.some((source) => source.deviceId === targetId)) {
                targetId = 'new-link'
            }
            if (!activeWork && !suppressWorkInference) activeWork = inferWork(next)
            void createQr()
        })
        void controller.initialize().catch((error) => { shareActionError = safeError(error) })
        refreshNotificationState()
        // Granting the permission happens in Android settings, so the warning
        // has to be re-read when the reader comes back rather than at mount only.
        window.addEventListener('focus', refreshNotificationState)
        const timer = setInterval(() => { now = Date.now() }, 1000)
        void createQr()
        return () => {
            clearInterval(timer)
            unsubscribe()
            window.removeEventListener('focus', refreshNotificationState)
        }
    })
</script>

{#snippet deviceRow(device: PeerEntry, direction: 'incoming' | 'outgoing')}
    {@const name = device.name || device.deviceId.slice(0, 8)}
    {@const DeviceIcon = deviceIcon(device.name)}
    {@const columnTitle = direction === 'outgoing' ? sync.devices.outgoingTitle : sync.devices.incomingTitle}
    <div class="flex items-center gap-3 px-4 py-2.5 text-sm">
        <span data-device-icon class="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-darkborderc bg-bgcolor" aria-hidden="true"><DeviceIcon size={18} /></span>
        <div class="min-w-0 flex-1">
            <p class="break-words">{name}</p>
            <p class="text-xs text-textcolor2 tabular-nums">{format(sync.devices.lastSeen, formatTime(device.lastSeenMs), formatRisuNestStorageBytes(device.totalBytes ?? 0))}</p>
            <div class="mt-1 flex flex-wrap gap-1">
                {#if device.permissions.includes('read')}<span class="rounded-md border border-darkborderc px-1.5 text-xs text-textcolor2">{sync.devices.permRead}</span>{/if}
                {#if device.permissions.includes('bidirectional')}<span class="rounded-md border border-darkborderc px-1.5 text-xs text-textcolor2">{sync.devices.permBidirectional}</span>{/if}
            </div>
        </div>
        <button type="button" aria-label={`${sync.devices.revoke}: ${name}, ${columnTitle}`} disabled={sourceTransition} class={revokeClass} onclick={() => revoke(direction, device.deviceId)}>{sync.devices.revoke}</button>
    </div>
{/snippet}

{#snippet tile(title: string, description: string, Icon: typeof Download, disabled: boolean, onclick: () => void)}
    <button type="button" data-label={title} {disabled} class={tileClass} {onclick}>
        <span class="flex items-center gap-1.5 text-sm font-bold"><Icon size={16} aria-hidden="true" />{title}</span>
        <span class="help text-xs leading-snug">{description}</span>
    </button>
{/snippet}

<div class="@container w-full max-w-3xl">
    <h1 class="text-2xl font-bold">{sync.menuTitle}</h1>
    <p data-sync-intro class="help mt-1 max-w-[62ch] text-sm">{sync.intro}</p>
    {#if isTauriAndroid && notificationsEnabled === false}
        <div class="mt-4 rounded-lg border border-draculared bg-darkbg p-4">
            <p data-notification-warning class="text-sm text-draculared">{sync.notificationsDisabledWarning}</p>
            <Button className="mt-2" size="sm" styled="outlined" onclick={openNotificationSettings}>{language.risuNest.platform.openSettings}</Button>
        </div>
    {/if}

    <SettingGroup id="sync-share" title={sync.share.title} panelProps={{ 'data-sync-card': 'sharing' }}>
        {#snippet actions()}
            {@const pill = statePill()}
            <span role="status" aria-live="polite" class="inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs font-semibold {pill.border}"><span class="h-2 w-2 rounded-full {pill.dot}" aria-hidden="true"></span>{stateLabel()}</span>
            <Button size="sm" disabled={sourceTransition || ['preparing', 'starting', 'stopping'].includes(snapshot.source.phase) || (!running && (receiveWorkBusy || !permissionsChosen))} styled={running ? 'danger' : 'primary'} onclick={startOrStop}>{running ? sync.share.stop : sync.share.start}</Button>
        {/snippet}
        {#if isTauriAndroid}
            <p class="help px-4 py-3 text-sm">{sync.androidLanOnly}</p>
        {:else}
            <SettingRow label={sync.share.method} help={methodHelp}>
                <SegmentedButtons role="radiogroup" label={sync.share.method} value={settings.syncListenMethod} options={methodOptions} onchange={(method) => updateSettings({ syncListenMethod: method })} />
            </SettingRow>
        {/if}
        {#if settings.syncListenMethod !== 'quick' || isTauriAndroid}
            <SettingRow label={sync.share.port} labelFor="device-sync-port">
                <input id="device-sync-port" type="number" min="1" max="65535" class="{inputClass} w-28 text-right tabular-nums" value={settings.syncFixedPort} oninput={(event) => { const port = Number(event.currentTarget.value); if (Number.isInteger(port) && port >= 1 && port <= 65535) updateSettings({ syncFixedPort: port }) }} />
            </SettingRow>
        {/if}
        {#if settings.syncListenMethod === 'fixed-url' && !isTauriAndroid}
            <SettingRow label={sync.share.publicUrl} labelFor="device-sync-public-url" help={sync.share.publicUrlHelp}>
                {#snippet below()}
                    <details class="mt-1 text-sm text-textcolor2"><summary class="cursor-pointer underline underline-offset-2">{sync.share.fixedGuideTitle}</summary><p class="mt-1">{format(sync.share.fixedGuideBody, settings.syncFixedPort)}</p></details>
                {/snippet}
                <input id="device-sync-public-url" type="url" inputmode="url" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" class="{inputClass} w-full @xl:w-64" value={settings.syncPublicBaseUrl} oninput={(event) => updateSettings({ syncPublicBaseUrl: event.currentTarget.value })} />
            </SettingRow>
        {/if}
        <SettingRow inline label={sync.share.autoListen}>
            <SettingToggle checked={settings.syncAutoListen} label={sync.share.autoListen} onchange={(checked) => updateSettings({ syncAutoListen: checked })} />
        </SettingRow>
        <div data-permissions class="divide-y divide-darkborderc/55">
            <SettingRow inline label={sync.share.permRead} help={sync.share.permReadHelp}>
                <SettingToggle checked={permissions.read} disabled={permissions.bidirectional} label={sync.share.permRead} onchange={(checked) => { permissions.read = checked }} />
            </SettingRow>
            <SettingRow inline label={sync.share.permBidirectional} help={sync.share.permBidirectionalHelp}>
                <SettingToggle checked={permissions.bidirectional} label={sync.share.permBidirectional} onchange={(checked) => { permissions.bidirectional = checked; if (checked) permissions.read = true }} />
            </SettingRow>
        </div>

        {#if running && pairUri}
            <div class="grid grid-cols-1 items-center gap-5 p-4 @xl:grid-cols-[auto_minmax(0,1fr)]">
                <div class="mx-auto flex h-44 w-44 items-center justify-center rounded-lg bg-white p-2 transition-opacity duration-300 @xl:mx-0" class:opacity-40={expired}>
                    {#if qrDataUrl}<img src={qrDataUrl} alt={sync.share.pairTitle} width="160" height="160" />{/if}
                </div>
                <div class="min-w-0 text-center @xl:text-left">
                    <h3 class="font-bold">{sync.share.pairTitle}</h3>
                    <p class="help mx-auto mt-1 max-w-[62ch] text-sm @xl:mx-0">{sync.share.pairNote}</p>
                    <p class="mt-2 text-sm tabular-nums">{expired ? sync.share.pairExpired : format(sync.share.pairRemaining, formatDuration(remaining))}</p>
                    <div class="mx-auto mt-1.5 h-1 max-w-80 overflow-hidden rounded-full bg-bgcolor @xl:mx-0" aria-hidden="true"><div class="h-full bg-success-500 transition-[width] duration-1000 ease-linear" style:width={`${remainingRatio * 100}%`}></div></div>
                    <div class="mt-3 flex flex-wrap justify-center gap-2 @xl:justify-start"><Button size="sm" disabled={expired} onclick={copyLink}>{sync.share.copyLink}</Button><Button size="sm" styled="outlined" disabled={sourceTransition || !permissionsChosen} onclick={rotate}>{sync.share.newLink}</Button></div>
                    {#if copyFailed && !expired}<p data-pair-uri class="mt-2 rounded-md border border-darkborderc bg-bgcolor p-2 text-xs break-all select-all">{pairUri}</p>{/if}
                </div>
            </div>
        {/if}
        {#if shareActionError}<p data-share-error role="alert" class="px-4 py-3 text-sm text-draculared">{shareActionError}</p>
        {:else if sourceError}<p role="alert" class="px-4 py-3 text-sm text-draculared">{safeError(sourceError)}</p>
        {:else if snapshot.remoteCommitNotice === 'editsDiscarded'}<p data-share-refresh role="alert" class="px-4 py-3 text-sm text-draculared">{sync.work.bidirectionalEditsDiscarded}</p>
        {:else if snapshot.remoteCommitNotice === 'refreshFailed'}<p data-share-refresh role="alert" class="px-4 py-3 text-sm text-draculared">{sync.work.bidirectionalRefreshPending}</p>{/if}
    </SettingGroup>

    <SettingGroup id="sync-devices" title={sync.devices.title} divide={false} panelProps={{ 'data-sync-card': 'devices' }}>
        <div class="grid grid-cols-1 @xl:grid-cols-2">
            <div>
                <h3 class="flex items-center gap-1.5 px-4 pt-3 pb-1 text-xs font-bold text-textcolor2"><ArrowUpRight size={14} aria-hidden="true" />{sync.devices.outgoingTitle}</h3>
                {#if snapshot.devices.length === 0}
                    <p class="px-4 pt-1 pb-4 text-sm text-textcolor2">{sync.devices.outgoingEmpty}</p>
                {:else}
                    <div class="divide-y divide-darkborderc/55">
                        {#each snapshot.devices as device (device.deviceId)}
                            {@render deviceRow(device, 'outgoing')}
                        {/each}
                    </div>
                {/if}
            </div>
            <div class="border-t border-darkborderc/55 @xl:border-t-0 @xl:border-l">
                <h3 class="flex items-center gap-1.5 px-4 pt-3 pb-1 text-xs font-bold text-textcolor2"><ArrowDownLeft size={14} aria-hidden="true" />{sync.devices.incomingTitle}</h3>
                {#if snapshot.sources.length === 0}
                    <p class="px-4 pt-1 pb-4 text-sm text-textcolor2">{sync.devices.incomingEmpty}</p>
                {:else}
                    <div class="divide-y divide-darkborderc/55">
                        {#each snapshot.sources as device (device.deviceId)}
                            {@render deviceRow(device, 'incoming')}
                        {/each}
                    </div>
                {/if}
            </div>
        </div>
    </SettingGroup>

    <SettingGroup id="sync-work" title={sync.work.title} panelProps={{ 'data-sync-card': 'work' }}>
        {#snippet actions()}
            <span class="text-xs text-textcolor2">{sync.work.backupNote}</span>
        {/snippet}
        <SettingRow label={sync.work.target} labelFor="device-sync-target">
            <select id="device-sync-target" class="{inputClass} w-full @xl:w-64" value={targetId} onchange={(event) => { targetId = event.currentTarget.value }}>
                {#each snapshot.sources as source (source.deviceId)}<option value={source.deviceId}>{source.name || source.deviceId.slice(0, 8)}</option>{/each}<option value="new-link">{sync.work.useNewLink}</option>
            </select>
        </SettingRow>
        {#if targetId === 'new-link'}
            <SettingRow label={sync.work.linkLabel} labelFor="device-sync-link" help={sync.work.linkHelp}>
                {#snippet below()}
                    {#if stagedLinkInvalid}<p data-link-invalid class="mt-1 text-sm text-draculared">{sync.work.linkInvalid}</p>{/if}
                {/snippet}
                <input id="device-sync-link" type="text" inputmode="url" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" aria-invalid={stagedLinkInvalid} class="{inputClass} w-full @xl:w-64" placeholder={sync.work.linkPlaceholder} value={stagedUri} oninput={(event) => updateStagedUri(event.currentTarget.value)} />
            </SettingRow>
        {/if}
        <div class="grid grid-cols-1 gap-2.5 p-4 @xl:grid-cols-3">
            {@render tile(sync.work.clone, sync.work.cloneHelp, Download, receiveBusy || !workAllowed('clone'), startClone)}
            {@render tile(sync.work.delta, sync.work.deltaHelp, RefreshCw, receiveBusy || !workAllowed('delta'), startDelta)}
            {@render tile(sync.work.bidirectional, sync.work.bidirectionalHelp, ArrowLeftRight, receiveBusy || !workAllowed('bidirectional'), startBidi)}
        </div>
        {#if sourceBusy}<p class="px-4 py-2.5 text-sm text-textcolor2">{sync.work.blockedWhileSharing}</p>{/if}
        {#if selectedExpired && !(activeWork && latestWorkError)}<p role="alert" class="px-4 py-2.5 text-sm text-draculared">{sync.registrationExpired}</p>{/if}

        {#if activeWork}
            <div data-work-status aria-live="polite" aria-busy={workActionPending} class="p-4">
                <p class="mb-2 text-sm font-bold">{laneTitle}</p>
                {#if activeWork === 'clone'}
                    {#if workActionPending || cloneTarget?.phase === 'confirmed'}<progress class="h-2 w-full accent-borderc" aria-label={sync.work.receiving}></progress><p class="mt-1 text-sm">{sync.work.receiving}</p>
                    {:else if cloneTarget?.phase === 'downloading'}
                        {#if cloneTarget.totalBytes !== undefined}
                            {@const percent = Math.round((cloneTarget.completedBytes / Math.max(1, cloneTarget.totalBytes)) * 100)}
                            <progress class="h-2 w-full accent-borderc" aria-label={format(sync.work.progress, percent)} value={cloneTarget.completedBytes} max={Math.max(1, cloneTarget.totalBytes)}></progress><p class="mt-1 text-sm tabular-nums">{format(sync.work.progress, percent)}</p>
                        {:else}<progress class="h-2 w-full accent-borderc" aria-label={sync.work.progressLabel}></progress><p class="mt-1 text-sm">{sync.work.progressLabel}</p>{/if}
                        <Button className="mt-2" size="sm" styled="danger" disabled={workActionPending} onclick={() => runWorkAction(() => controller.cancelClone())}>{sync.work.cancel}</Button>
                    {:else if cloneRetryable}
                        <div class="flex flex-wrap gap-2">
                            <Button size="sm" disabled={workActionPending} onclick={() => runWorkAction(() => controller.resumeClone())}>{sync.work.resume}</Button>
                            {#if cloneTerminal}<Button size="sm" styled="danger" disabled={workActionPending} onclick={dismissClone}>{sync.work.dismiss}</Button>{/if}
                        </div>
                    {:else if cloneTarget?.phase === 'failed' || cloneTarget?.phase === 'cancelled'}
                        <Button className="mt-2" size="sm" disabled={workActionPending} onclick={dismissClone}>{sync.work.dismiss}</Button>
                    {:else if cloneTarget?.phase === 'completed'}
                        {@const backupPaths = cloneBackupPaths()}
                        <p class="text-sm">{format(sync.work.doneUpdated, 1, formatRisuNestStorageBytes(cloneTarget.totalBytes ?? cloneTarget.completedBytes))}</p>
                        {#if backupPaths.length > 0}<details class="mt-2 text-sm text-textcolor2"><summary class="cursor-pointer underline underline-offset-2">{sync.work.backupLocation}</summary><ul class="mt-1 list-inside list-disc">{#each backupPaths as path}<li class="break-all">{path}</li>{/each}</ul></details>{/if}
                        <Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {/if}
                {:else if activeWork === 'delta'}
                    {#if deltaRetained && delta?.pullPhase !== 'running'}
                        <p data-delta-retained class="text-sm">{format(sync.work.deltaRetained, deltaRetained.sourceName || sync.work.unknownDevice)}</p>
                        <p class="mt-1 text-sm text-textcolor2">{deltaRetained.witness === 'ambiguous' ? sync.work.deltaRetainedAmbiguous : sync.work.deltaRetainedResumable}</p>
                        <div class="mt-2 flex flex-wrap gap-2">
                            {#if deltaResumable}
                                <Button size="sm" disabled={workActionPending || sourceBusy} onclick={resumeRetainedDelta}>{sync.work.resume}</Button>
                            {/if}
                            <Button size="sm" styled="danger" disabled={workActionPending || sourceBusy} onclick={abandonRetainedDelta}>{sync.work.abandon}</Button>
                        </div>
                    {:else if workActionPending || delta?.pullPhase === 'running'}<progress class="h-2 w-full accent-borderc" aria-label={sync.work.receiving}></progress><p class="mt-1 text-sm">{sync.work.receiving}</p>
                    {:else if delta?.pullPhase === 'completed' && delta.pullResult?.kind === 'noChanges'}<p class="text-sm">{sync.work.doneUpToDate}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'completed' && delta.pullResult?.kind === 'updated'}<p class="text-sm">{format(sync.work.doneUpdated, delta.pullResult.transferredObjects, formatRisuNestStorageBytes(delta.pullResult.transferredBytes))}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'fullCloneRequired'}<p class="text-sm">{sync.work.needClone}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'conflict'}<p class="text-sm text-draculared">{delta.pullResult?.kind === 'conflict' && delta.pullResult.reason === 'staleRevision' ? sync.work.deltaConflictLocalChanged : sync.work.deltaConflictBothChanged}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'failed'}<Button size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>{/if}
                {:else}
                    {#if workActionPending || bidiPhase === 'running'}<p class="text-sm">{sync.work.bidirectionalSyncing}</p>
                    {:else if bidiPhase === 'awaitingConflict'}
                        {@const conflict = conflictNames()}
                        <div class="rounded-lg border border-draculared p-3.5"><p class="font-bold text-draculared">{sync.work.conflictTitle}</p><p class="help mt-1 text-sm">{sync.work.conflictBody}</p><ul class="mt-2 list-inside list-disc text-sm">{#each conflict.names as name}<li>{name}</li>{/each}{#if conflict.otherCount > 0}<li>{format(sync.work.conflictOthers, conflict.otherCount)}</li>{/if}</ul><div class="mt-2 flex flex-wrap gap-2"><Button size="sm" disabled={!conflictSourceId} onclick={() => resolve('local')}>{sync.work.keepThis}</Button><Button size="sm" disabled={!conflictSourceId} onclick={() => resolve('remote')}>{sync.work.keepOther}</Button></div>{#if !conflictSourceId}<p data-conflict-target class="mt-2 text-sm text-draculared">{sync.work.conflictSelectTarget}</p>{/if}</div>
                    {:else if bidiPhase === 'sourcePrepared'}<p class="text-sm text-textcolor2">{sync.share.start}: {sync.work.bidirectionalResumeRequired}</p><div class="mt-2 flex gap-2"><Button size="sm" disabled={!['idle', 'error', 'prepared'].includes(snapshot.source.phase)} onclick={resumeBidi}>{sync.work.resume}</Button>{#if snapshot.source.phase === 'idle'}<Button size="sm" styled="danger" onclick={abandon}>{sync.work.abandon}</Button>{/if}</div>
                    {:else if ['sourceUnavailable', 'localCommitted', 'targetPrepared'].includes(bidiPhase)}<p class="text-sm text-textcolor2">{bidiPhase === 'sourceUnavailable' ? sync.work.bidirectionalSourceUnavailable : sync.work.bidirectionalResumeRequired}</p><div class="mt-2 flex gap-2"><Button size="sm" onclick={resumeBidi}>{sync.work.resume}</Button><Button size="sm" styled="danger" onclick={abandon}>{sync.work.abandon}</Button></div>
                    {:else if bidiPhase === 'refreshPending'}<p class="text-sm text-textcolor2">{sync.work.bidirectionalRefreshPending}</p><Button className="mt-2" size="sm" onclick={resumeBidi}>{sync.work.resume}</Button>
                    {:else if bidiPhase === 'failed'}<Button size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if bidiPhase === 'completed' && bidi?.operationResult?.kind !== 'conflict'}
                        {@const result = bidi.operationResult}
                        <p class="text-sm">{result?.kind === 'noChanges' ? sync.work.doneUpToDate : result?.kind === 'updated' ? format(sync.work.doneUpdated, result.transferredObjects, formatRisuNestStorageBytes(result.transferredBytes)) : sync.work.doneUpToDate}</p>
                        {#if result && 'backups' in result && result.backups.length > 0}<details class="mt-2 text-sm text-textcolor2"><summary class="cursor-pointer underline underline-offset-2">{sync.work.backupLocation}</summary><ul class="mt-1 list-inside list-disc">{#each result.backups as backup (backup.packageId)}<li class="break-all">{backup.path}</li>{/each}</ul></details>{/if}
                        <Button className="mt-2" size="sm" onclick={acknowledgeBidi}>{sync.work.dismiss}</Button>
                    {/if}
                {/if}
                {#if latestWorkError}<p data-work-error role="alert" class="mt-2 text-sm text-draculared">{latestWorkError}</p>{/if}
            </div>
        {/if}
    </SettingGroup>
</div>

<style>
    .help {
        color: color-mix(in srgb, var(--risu-theme-textcolor2) 62%, var(--risu-theme-textcolor) 38%);
    }
</style>
