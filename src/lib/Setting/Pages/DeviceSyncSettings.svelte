<script lang="ts">
    import { onMount } from 'svelte'
    import { MonitorSmartphone } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { alertConfirm } from 'src/ts/alert'
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

    type WorkLane = 'clone' | 'delta' | 'bidirectional'

    const controller = getProductionDeviceSyncController()
    const sync = language.risuNest.sync
    const initialSnapshot = controller.snapshot()
    let snapshot = $state<DeviceSyncControllerSnapshot>(initialSnapshot)
    let settings = $state(getDeviceSettings())
    let permissions = $state({ read: true, bidirectional: false })
    let targetId = $state('new-link')
    let stagedUri = $state(initialSnapshot.stagedUri ?? '')
    let observedStagedUri = initialSnapshot.stagedUri
    let qrDataUrl = $state('')
    let qrUri = ''
    let now = $state(Date.now())
    let notificationsEnabled = $state<boolean | null>(null)
    let sourceTransition = $state(false)
    let activeWork = $state<WorkLane | null>(null)
    let suppressWorkInference = false
    let workActionPending = $state(false)
    let shareActionError = $state<string | null>(null)
    let workActionError = $state<string | null>(null)

    const sourceBusy = $derived(['preparing', 'prepared', 'starting', 'running', 'stopping'].includes(snapshot.source.phase))
    const sourceError = $derived(snapshot.source.latestError ?? snapshot.sourceError)
    const pairUri = $derived(snapshot.source.pairingUri ?? '')
    const expired = $derived(!snapshot.source.expiresAtMs || snapshot.source.expiresAtMs <= now)
    const remaining = $derived(snapshot.source.expiresAtMs ? Math.max(0, snapshot.source.expiresAtMs - now) : 0)
    const clone = $derived(snapshot.targets.clone)
    const delta = $derived(snapshot.targets.delta)
    const bidi = $derived(snapshot.targets.bidirectional)
    const cloneTarget = $derived(clone?.state.target)
    const bidiPhase = $derived(bidi?.operationPhase ?? 'idle')
    const selectedIncoming = $derived(snapshot.sources.find((source) => source.deviceId === targetId))
    const selectedExpired = $derived(targetId !== 'new-link' && snapshot.expiredSourceIds.includes(targetId))
    const cloneRetryable = $derived(Boolean(clone?.resumeAvailable) || (
        !isTauriAndroid && (cloneTarget?.phase === 'cancelled' || cloneTarget?.phase === 'failed')
    ))
    const cloneBusy = $derived(
        workActionPending && activeWork === 'clone'
        || cloneTarget?.phase === 'downloading'
        || cloneTarget?.phase === 'confirmed'
        || cloneRetryable
    )
    const deltaRetained = $derived(delta?.retained ?? null)
    const deltaResumable = $derived(Boolean(
        deltaRetained
        && deltaRetained.witness !== 'ambiguous'
        && snapshot.sources.some((source) => source.deviceId === deltaRetained.sourceDeviceId)
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
    function stateColor(): string {
        if (sourceError || snapshot.source.phase === 'error') return 'bg-draculared'
        if (snapshot.source.phase === 'running') return 'bg-selected'
        if (['prepared', 'preparing', 'starting', 'stopping'].includes(snapshot.source.phase)) return 'bg-darkbutton'
        return 'bg-textcolor2'
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
        if (safeCode === 'invalid-configuration') return sync.share.errorInvalidConfiguration
        if (safeCode === 'cleanup-failed') return sync.share.errorCleanupFailed
        return sync.share.stateError
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
    async function startOrStop(): Promise<void> {
        if (sourceTransition) return
        sourceTransition = true
        await runAction(async () => {
            if (snapshot.source.phase === 'running') { await controller.stop(); return }
            if (snapshot.source.phase !== 'prepared') await controller.prepare(sourceRequest())
            await controller.start(permissions)
        })
        sourceTransition = false
    }
    async function copyLink(): Promise<void> {
        if (pairUri && !expired) await runAction(() => navigator.clipboard.writeText(pairUri))
    }
    async function rotate(): Promise<void> {
        if (await runAction(() => controller.rotateLink(permissions))) await createQr()
    }
    async function revoke(direction: 'incoming' | 'outgoing', deviceId: string): Promise<void> {
        if (!await alertConfirm(sync.devices.revokeConfirm)) return
        const removed = await runAction(() => direction === 'incoming' ? controller.revokeIncoming(deviceId) : controller.revokeOutgoing(deviceId))
        if (removed && direction === 'incoming' && targetId === deviceId) targetId = 'new-link'
    }
    async function beginWork(lane: WorkLane, action: () => Promise<unknown>): Promise<void> {
        if (receiveBusy || !workAllowed(lane)) return
        suppressWorkInference = false
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
            try {
                parseDeviceSyncUri(stagedUri)
                return true
            } catch {
                return false
            }
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
        if (!await alertConfirm(sync.work.cloneConfirm)) return
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
        const sourceId = snapshot.activeBidirectionalSourceDeviceId ?? snapshot.stagedSourceDeviceId ?? selectedIncoming?.deviceId
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
        if (workActionPending) return
        workActionPending = true
        try {
            const confirmed = await alertConfirm(sync.work.abandonConfirm)
            if (confirmed && await runAction(() => controller.abandonBidirectional(), 'work')) dismissWork()
        } finally {
            workActionPending = false
        }
    }
    async function resumeRetainedDelta(): Promise<void> {
        const sourceDeviceId = deltaRetained?.sourceDeviceId
        if (!sourceDeviceId) return
        await runWorkAction(() => controller.pullRegisteredDelta(sourceDeviceId))
    }
    async function abandonRetainedDelta(): Promise<void> {
        if (workActionPending) return
        workActionPending = true
        try {
            const confirmed = await alertConfirm(sync.work.deltaAbandonConfirm)
            if (confirmed && await runAction(() => controller.abandonDelta(), 'work')) dismissWork()
        } finally {
            workActionPending = false
        }
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
        void controller.initialize().catch(() => { shareActionError = sync.share.stateError })
        notificationsEnabled = androidPeerSyncNotificationsEnabled()
        const timer = setInterval(() => { now = Date.now() }, 1000)
        void createQr()
        return () => { clearInterval(timer); unsubscribe() }
    })
</script>

<section class="mt-4">
    <h3 class="text-xl font-bold">{sync.menuTitle}</h3>
    <p data-sync-intro class="mt-1 text-sm text-textcolor2">{sync.intro}</p>
    {#if isTauriAndroid && notificationsEnabled === false}
        <p data-notification-warning class="mt-3 rounded-md border border-draculared bg-darkbg p-3 text-sm text-draculared">{language.peerClone.notificationsDisabledWarning}</p>
    {/if}
    <div data-sync-card="sharing" class="mt-4 rounded-md border border-darkborderc bg-darkbg p-4">
        <div class="flex items-center justify-between gap-3">
            <h4 class="font-bold">{sync.share.title}</h4>
            <span role="status" aria-live="polite" class="flex items-center gap-2 text-sm"><span class="h-2 w-2 rounded-full {stateColor()}"></span>{stateLabel()}</span>
        </div>
        {#if isTauriAndroid}
            <p class="mt-3 text-sm text-textcolor2">{sync.androidLanOnly}</p>
        {:else}
            <div class="mt-3 flex flex-wrap gap-2" role="radiogroup" aria-label={sync.share.title}>
                {#each [['lan', sync.share.methodLan], ['quick', sync.share.methodQuick], ['fixed-url', sync.share.methodFixed]] as [method, label]}
                    <button type="button" role="radio" aria-checked={settings.syncListenMethod === method} class="rounded-md border border-darkborderc px-2 py-1 text-sm text-textcolor2 transition-colors hover:bg-darkbg focus:outline-hidden focus:ring-2 focus:ring-selected {settings.syncListenMethod === method ? 'bg-darkbutton' : 'bg-transparent'}" onclick={() => updateSettings({ syncListenMethod: method as typeof settings.syncListenMethod })}>{label}</button>
                {/each}
            </div>
        {/if}
        {#if settings.syncListenMethod !== 'quick' || isTauriAndroid}
            <label class="mt-3 block text-sm font-bold" for="device-sync-port">{sync.share.port}</label>
            <input id="device-sync-port" type="number" min="1" max="65535" class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={settings.syncFixedPort} oninput={(event) => { const port = Number(event.currentTarget.value); if (Number.isInteger(port) && port >= 1 && port <= 65535) updateSettings({ syncFixedPort: port }) }} />
        {:else}
            <p class="mt-3 text-sm text-textcolor2">{sync.share.quickNote}</p>
        {/if}
        {#if settings.syncListenMethod === 'fixed-url' && !isTauriAndroid}
            <label class="mt-3 block text-sm font-bold" for="device-sync-public-url">{sync.share.publicUrl}</label>
            <input id="device-sync-public-url" type="url" class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={settings.syncPublicBaseUrl} oninput={(event) => updateSettings({ syncPublicBaseUrl: event.currentTarget.value })} />
            <p class="mt-2 text-sm text-textcolor2">{sync.share.fixedNote}</p>
            <details class="mt-2 text-sm text-textcolor2"><summary class="cursor-pointer">{sync.share.fixedGuideTitle}</summary><p class="mt-2">{format(sync.share.fixedGuideBody, settings.syncFixedPort)}</p></details>
        {/if}
        <label class="mt-3 flex gap-2 text-sm"><input type="checkbox" checked={settings.syncAutoListen} onchange={(event) => updateSettings({ syncAutoListen: event.currentTarget.checked })} />{sync.share.autoListen}</label>
        <div data-permissions class="mt-3">
            <label class="flex gap-2 text-sm"><input type="checkbox" checked={permissions.read} disabled={permissions.bidirectional} onchange={(event) => { permissions.read = event.currentTarget.checked }} />{sync.share.permRead}</label>
            <label class="mt-2 flex gap-2 text-sm"><input type="checkbox" checked={permissions.bidirectional} onchange={(event) => { permissions.bidirectional = event.currentTarget.checked; if (permissions.bidirectional) permissions.read = true }} />{sync.share.permBidirectional}</label>
            <p class="mt-1 text-xs text-textcolor2">{sync.share.permBidirectionalHelp}</p>
        </div>
        <Button className="mt-3" disabled={sourceTransition || ['preparing', 'starting', 'stopping'].includes(snapshot.source.phase) || (snapshot.source.phase !== 'running' && receiveWorkBusy)} styled={snapshot.source.phase === 'running' ? 'danger' : 'primary'} onclick={startOrStop}>{snapshot.source.phase === 'running' ? sync.share.stop : sync.share.start}</Button>

        {#if snapshot.source.phase === 'running' && pairUri}
            <div class="mt-4 border-t border-darkborderc pt-4">
                <div class="mt-3 flex flex-wrap gap-4">
                    {#if qrDataUrl}<img class:opacity-40={expired} src={qrDataUrl} alt={sync.share.pairTitle} width="192" height="192" />{/if}
                    <div class="min-w-0 flex-1">
                        <h5 class="font-bold">{sync.share.pairTitle}</h5>
                        <p class="mt-1 text-sm text-textcolor2">{sync.share.pairNote}</p>
                        <p class="mt-2 text-sm">{expired ? sync.share.pairExpired : format(sync.share.pairRemaining, formatDuration(remaining))}</p>
                        <div class="mt-2 flex flex-wrap gap-2"><Button size="sm" disabled={expired} onclick={copyLink}>{sync.share.copyLink}</Button><Button size="sm" styled="outlined" onclick={rotate}>{sync.share.newLink}</Button></div>
                    </div>
                </div>
            </div>
        {/if}
        {#if shareActionError}<p data-share-error role="alert" class="mt-3 text-sm text-draculared">{shareActionError}</p>
        {:else if sourceError}<p role="alert" class="mt-3 text-sm text-draculared">{safeError(sourceError)}</p>{/if}
    </div>

    <div data-sync-card="devices" class="mt-4 rounded-md border border-darkborderc bg-darkbg p-4">
        <h4 class="font-bold">{sync.devices.title}</h4><p class="mt-1 text-sm text-textcolor2">{sync.devices.note}</p>
        <div class="mt-4"><h5 class="font-bold">{sync.devices.outgoingTitle}</h5>
            {#if snapshot.devices.length === 0}<p class="mt-2 text-sm text-textcolor2">{sync.devices.outgoingEmpty}</p>{:else}
                {#each snapshot.devices as device (device.deviceId)}
                    <div class="mt-2 flex items-center justify-between gap-3 border-t border-darkborderc pt-2 text-sm">
                        <div class="flex min-w-0 items-center gap-3"><span data-device-icon aria-hidden="true"><MonitorSmartphone size={18} /></span><div><p>{device.name || device.deviceId.slice(0, 8)}</p><p class="text-textcolor2">{format(sync.devices.lastSeen, formatTime(device.lastSeenMs), formatRisuNestStorageBytes(device.totalBytes ?? 0))}</p><div class="mt-1 flex gap-1">{#if device.permissions.includes('read')}<span class="rounded-md border border-darkborderc px-1.5 text-xs">{sync.devices.permRead}</span>{/if}{#if device.permissions.includes('bidirectional')}<span class="rounded-md border border-darkborderc px-1.5 text-xs">{sync.devices.permBidirectional}</span>{/if}</div></div></div>
                        <button type="button" aria-label={`${sync.devices.revoke}: ${device.name || device.deviceId.slice(0, 8)}, ${sync.devices.outgoingTitle}`} class="rounded-md border border-draculared bg-draculared/80 px-2 py-1 text-sm text-textcolor shadow-xs transition-colors duration-200 hover:bg-draculared focus:outline-hidden focus:ring-2 focus:ring-draculared" onclick={() => revoke('outgoing', device.deviceId)}>{sync.devices.revoke}</button>
                    </div>
                {/each}
            {/if}
        </div>
        <div class="mt-4"><h5 class="font-bold">{sync.devices.incomingTitle}</h5>
            {#if snapshot.sources.length === 0}<p class="mt-2 text-sm text-textcolor2">{sync.devices.incomingEmpty}</p>{:else}
                {#each snapshot.sources as device (device.deviceId)}
                    <div class="mt-2 flex items-center justify-between gap-3 border-t border-darkborderc pt-2 text-sm">
                        <div class="flex min-w-0 items-center gap-3"><span data-device-icon aria-hidden="true"><MonitorSmartphone size={18} /></span><div><p>{device.name || device.deviceId.slice(0, 8)}</p><p class="text-textcolor2">{format(sync.devices.lastSeen, formatTime(device.lastSeenMs), formatRisuNestStorageBytes(device.totalBytes ?? 0))}</p><div class="mt-1 flex gap-1">{#if device.permissions.includes('read')}<span class="rounded-md border border-darkborderc px-1.5 text-xs">{sync.devices.permRead}</span>{/if}{#if device.permissions.includes('bidirectional')}<span class="rounded-md border border-darkborderc px-1.5 text-xs">{sync.devices.permBidirectional}</span>{/if}</div></div></div>
                        <button type="button" aria-label={`${sync.devices.revoke}: ${device.name || device.deviceId.slice(0, 8)}, ${sync.devices.incomingTitle}`} class="rounded-md border border-draculared bg-draculared/80 px-2 py-1 text-sm text-textcolor shadow-xs transition-colors duration-200 hover:bg-draculared focus:outline-hidden focus:ring-2 focus:ring-draculared" onclick={() => revoke('incoming', device.deviceId)}>{sync.devices.revoke}</button>
                    </div>
                {/each}
            {/if}
        </div>
    </div>

    <div data-sync-card="work" class="mt-4 rounded-md border border-darkborderc bg-darkbg p-4">
        <h4 class="font-bold">{sync.work.title}</h4>
        <label class="mt-3 block text-sm font-bold" for="device-sync-target">{sync.work.target}</label>
        <select id="device-sync-target" class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={targetId} onchange={(event) => { targetId = event.currentTarget.value }}>
            {#each snapshot.sources as source (source.deviceId)}<option value={source.deviceId}>{source.name || source.deviceId.slice(0, 8)}</option>{/each}<option value="new-link">{sync.work.useNewLink}</option>
        </select>
        {#if targetId === 'new-link'}<label class="sr-only" for="device-sync-link">{sync.work.linkPlaceholder}</label><input id="device-sync-link" class="mt-2 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" placeholder={sync.work.linkPlaceholder} value={stagedUri} oninput={(event) => updateStagedUri(event.currentTarget.value)} />{/if}
        <div class="mt-3 flex flex-col gap-2 sm:flex-row"><Button disabled={receiveBusy || !workAllowed('clone')} onclick={startClone}>{sync.work.clone}</Button><Button disabled={receiveBusy || !workAllowed('delta')} onclick={startDelta}>{sync.work.delta}</Button><Button disabled={receiveBusy || !workAllowed('bidirectional')} onclick={startBidi}>{sync.work.bidirectional}</Button></div>
        {#if sourceBusy}<p class="mt-2 text-sm text-textcolor2">{sync.work.blockedWhileSharing}</p>{/if}
        {#if selectedExpired && !(activeWork && latestWorkError)}<p role="alert" class="mt-2 text-sm text-draculared">{sync.registrationExpired}</p>{/if}

        {#if activeWork}
            <div data-work-status aria-live="polite" class="mt-4 border-t border-darkborderc pt-4">
                {#if activeWork === 'clone'}
                    {#if workActionPending || cloneTarget?.phase === 'confirmed'}<p class="text-sm">{format(sync.work.progress, 0)}</p>
                    {:else if cloneTarget?.phase === 'downloading'}
                        {#if cloneTarget.totalBytes !== undefined}
                            {@const percent = Math.round((cloneTarget.completedBytes / Math.max(1, cloneTarget.totalBytes)) * 100)}
                            <progress class="w-full" aria-label={format(sync.work.progress, percent)} value={cloneTarget.completedBytes} max={Math.max(1, cloneTarget.totalBytes)}></progress><p class="mt-1 text-sm">{format(sync.work.progress, percent)}</p>
                        {:else}<progress class="w-full" aria-label={language.peerClone.progress}></progress><p class="mt-1 text-sm">{language.peerClone.progress}</p>{/if}
                        <Button className="mt-2" size="sm" styled="danger" disabled={workActionPending} onclick={() => runWorkAction(() => controller.cancelClone())}>{sync.work.cancel}</Button>
                    {:else if cloneRetryable}
                        <Button size="sm" disabled={workActionPending} onclick={() => runWorkAction(() => controller.resumeClone())}>{sync.work.resume}</Button>
                    {:else if cloneTarget?.phase === 'failed' || cloneTarget?.phase === 'cancelled'}
                        <Button className="mt-2" size="sm" disabled={workActionPending} onclick={dismissClone}>{sync.work.dismiss}</Button>
                    {:else if cloneTarget?.phase === 'completed'}
                        {@const backupPaths = cloneBackupPaths()}
                        <p class="text-sm">{format(sync.work.doneUpdated, 1, formatRisuNestStorageBytes(cloneTarget.totalBytes ?? cloneTarget.completedBytes))}</p>
                        {#if backupPaths.length > 0}<p class="mt-2 text-sm text-textcolor2">{sync.work.backupNote}</p><details class="mt-2 text-sm text-textcolor2"><summary class="cursor-pointer">{sync.work.backupNote}</summary><ul class="mt-1 list-inside list-disc">{#each backupPaths as path}<li>{path}</li>{/each}</ul></details>{/if}
                        <Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {/if}
                {:else if activeWork === 'delta'}
                    {#if deltaRetained && delta?.pullPhase !== 'running'}
                        <p data-delta-retained class="text-sm">{format(sync.work.deltaRetained, deltaRetained.sourceName || sync.work.unknownDevice)}</p>
                        <p class="mt-1 text-sm text-textcolor2">{deltaRetained.witness === 'ambiguous' ? sync.work.deltaRetainedAmbiguous : sync.work.deltaRetainedResumable}</p>
                        <div class="mt-2 flex flex-wrap gap-2">
                            {#if deltaResumable}
                                <Button size="sm" disabled={workActionPending} onclick={resumeRetainedDelta}>{sync.work.resume}</Button>
                            {/if}
                            <Button size="sm" styled="danger" disabled={workActionPending} onclick={abandonRetainedDelta}>{sync.work.abandon}</Button>
                        </div>
                    {:else if workActionPending || delta?.pullPhase === 'running'}<p class="text-sm">{format(sync.work.progress, 0)}</p>
                    {:else if delta?.pullPhase === 'completed' && delta.pullResult?.kind === 'noChanges'}<p class="text-sm">{sync.work.doneUpToDate}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'completed' && delta.pullResult?.kind === 'updated'}<p class="text-sm">{format(sync.work.doneUpdated, delta.pullResult.transferredObjects, formatRisuNestStorageBytes(delta.pullResult.transferredBytes))}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'fullCloneRequired'}<p class="text-sm">{sync.work.needClone}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'conflict'}<p class="text-sm text-draculared">{delta.pullResult?.kind === 'conflict' && delta.pullResult.reason === 'staleRevision' ? language.peerDelta.conflictStaleRevision : language.peerDelta.conflictLocalRemote}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if delta?.pullPhase === 'failed'}<Button size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>{/if}
                {:else}
                    {#if workActionPending || bidiPhase === 'running'}<p class="text-sm">{language.peerBidirectional.syncing}</p>
                    {:else if bidiPhase === 'awaitingConflict'}
                        {@const conflict = conflictNames()}
                        <div class="rounded-md border border-draculared p-3"><p class="font-bold text-draculared">{sync.work.conflictTitle}</p><p class="mt-1 text-sm text-textcolor2">{sync.work.conflictBody}</p><ul class="mt-2 list-inside list-disc text-sm">{#each conflict.names as name}<li>{name}</li>{/each}{#if conflict.otherCount > 0}<li>{format(sync.work.conflictOthers, conflict.otherCount)}</li>{/if}</ul><div class="mt-2 flex flex-wrap gap-2"><Button size="sm" onclick={() => resolve('local')}>{sync.work.keepThis}</Button><Button size="sm" onclick={() => resolve('remote')}>{sync.work.keepOther}</Button></div></div>
                    {:else if bidiPhase === 'sourcePrepared'}<p class="text-sm text-textcolor2">{sync.share.start}: {language.peerBidirectional.resumeRequired}</p><div class="mt-2 flex gap-2"><Button size="sm" disabled={!['idle', 'error', 'prepared'].includes(snapshot.source.phase)} onclick={resumeBidi}>{sync.work.resume}</Button>{#if snapshot.source.phase === 'idle'}<Button size="sm" styled="danger" onclick={abandon}>{sync.work.abandon}</Button>{/if}</div>
                    {:else if ['sourceUnavailable', 'localCommitted', 'targetPrepared'].includes(bidiPhase)}<p class="text-sm text-textcolor2">{bidiPhase === 'sourceUnavailable' ? language.peerBidirectional.sourceUnavailable : language.peerBidirectional.resumeRequired}</p><div class="mt-2 flex gap-2"><Button size="sm" onclick={resumeBidi}>{sync.work.resume}</Button><Button size="sm" styled="danger" onclick={abandon}>{sync.work.abandon}</Button></div>
                    {:else if bidiPhase === 'refreshPending'}<p class="text-sm text-textcolor2">{language.peerBidirectional.refreshPending}</p><Button className="mt-2" size="sm" onclick={resumeBidi}>{sync.work.resume}</Button>
                    {:else if bidiPhase === 'failed'}<Button size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if bidiPhase === 'stale'}<p class="text-sm">{language.peerBidirectional.stale}</p><Button className="mt-2" size="sm" onclick={dismissWork}>{sync.work.dismiss}</Button>
                    {:else if bidiPhase === 'completed' && bidi?.operationResult?.kind !== 'conflict'}
                        {@const result = bidi.operationResult}
                        <p class="text-sm">{result?.kind === 'noChanges' ? sync.work.doneUpToDate : result?.kind === 'updated' ? format(sync.work.doneUpdated, result.transferredObjects, formatRisuNestStorageBytes(result.transferredBytes)) : sync.work.doneUpToDate}</p>
                        {#if result && 'backups' in result && result.backups.length > 0}<p class="mt-2 text-sm text-textcolor2">{sync.work.backupNote}</p><details class="mt-2 text-sm text-textcolor2"><summary class="cursor-pointer">{sync.work.backupNote}</summary><ul class="mt-1 list-inside list-disc">{#each result.backups as backup (backup.packageId)}<li>{backup.path}</li>{/each}</ul></details>{/if}
                        <Button className="mt-2" size="sm" onclick={acknowledgeBidi}>{sync.work.dismiss}</Button>
                    {/if}
                {/if}
                {#if latestWorkError}<p data-work-error role="alert" class="mt-2 text-sm text-draculared">{latestWorkError}</p>{/if}
            </div>
        {/if}
    </div>
    <p data-lan-warning class="mt-4 rounded-md border border-darkborderc bg-selected p-3 text-sm text-textcolor">{sync.lanWarning}</p>
</section>
