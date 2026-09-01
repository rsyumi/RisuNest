<script lang="ts">
    import { onMount } from 'svelte'
    import { language } from 'src/lang'
    import { alertConfirm } from 'src/ts/alert'
    import { isTauriAndroid } from 'src/ts/platform'
    import { getDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'
    import { getProductionDeviceSyncController } from 'src/ts/storage/sync/deviceSyncProduction'
    import type { DeviceSyncControllerSnapshot } from 'src/ts/storage/sync/deviceSyncController'
    import Button from 'src/lib/UI/GUI/Button.svelte'

    const controller = getProductionDeviceSyncController()
    const sync = language.risuNest.sync
    let snapshot = $state<DeviceSyncControllerSnapshot>(controller.snapshot())
    let settings = $state(getDeviceSettings())
    let permissions = $state({ read: true, bidirectional: false })
    let targetId = $state('new-link')
    let stagedUri = $state('')
    let qrDataUrl = $state('')
    let now = $state(Date.now())
    const sharing = $derived(snapshot.source.phase !== 'idle')
    const sourceError = $derived(snapshot.source.latestError ?? snapshot.error)
    const pairUri = $derived(snapshot.source.pairingUri ?? '')
    const expired = $derived(!snapshot.source.expiresAtMs || snapshot.source.expiresAtMs <= now)
    const remaining = $derived(snapshot.source.expiresAtMs ? Math.max(0, snapshot.source.expiresAtMs - now) : 0)
    const clone = $derived(snapshot.targets.clone)
    const delta = $derived(snapshot.targets.delta)
    const bidi = $derived(snapshot.targets.bidirectional)
    const cloneTarget = $derived(clone?.state.target)
    const bidiPhase = $derived(bidi?.operationPhase ?? 'idle')
    const selectedIncoming = $derived(snapshot.sources.find((source) => source.deviceId === targetId))

    function stateLabel(): string {
        if (sourceError || snapshot.source.phase === 'error') return sync.share.stateError
        if (snapshot.source.phase === 'running') return sync.share.stateListening
        if (snapshot.source.phase === 'stopping') return sync.share.stateStopping
        if (snapshot.source.phase === 'prepared' || snapshot.source.phase === 'preparing' || snapshot.source.phase === 'starting') return sync.share.statePreparing
        return sync.share.stateOff
    }
    function stateColor(): string {
        if (sourceError || snapshot.source.phase === 'error') return 'bg-draculared'
        if (snapshot.source.phase === 'running') return 'bg-green-500'
        if (snapshot.source.phase === 'prepared' || snapshot.source.phase === 'preparing' || snapshot.source.phase === 'starting' || snapshot.source.phase === 'stopping') return 'bg-yellow-400'
        return 'bg-textcolor2'
    }
    function updateSettings(partial: Partial<typeof settings>): void { updateDeviceSettings(partial); settings = getDeviceSettings() }
    async function createQr(): Promise<void> {
        if (!pairUri || expired) { qrDataUrl = ''; return }
        const { default: QRCode } = await import('qrcode')
        qrDataUrl = await QRCode.toDataURL(pairUri, { margin: 1, width: 192 })
    }
    async function startOrStop(): Promise<void> {
        if (snapshot.source.phase === 'running') { await controller.stop(); return }
        await controller.prepare({ method: isTauriAndroid ? 'lan' : settings.syncListenMethod, fixedPort: settings.syncFixedPort, publicBaseUrl: settings.syncPublicBaseUrl })
        await controller.start(permissions)
    }
    async function copyLink(): Promise<void> { if (pairUri && !expired) await navigator.clipboard.writeText(pairUri) }
    async function rotate(): Promise<void> { await controller.rotateLink(permissions); await createQr() }
    async function revoke(direction: 'incoming' | 'outgoing', deviceId: string): Promise<void> {
        if (!await alertConfirm(sync.devices.revokeConfirm)) return
        if (direction === 'incoming') await controller.revokeIncoming(deviceId)
        else await controller.revokeOutgoing(deviceId)
    }
    async function chooseTarget(value: string): Promise<void> { targetId = value; if (value !== 'new-link') await controller.selectRegisteredClone(value) }
    async function startClone(): Promise<void> {
        if (!await alertConfirm(sync.work.cloneConfirm)) return
        if (targetId === 'new-link') { controller.stageLink(stagedUri); await controller.claimStagedClone() }
        await controller.confirmCloneReplace(); await controller.downloadClone()
    }
    async function startDelta(): Promise<void> { if (targetId === 'new-link') { controller.stageLink(stagedUri); await controller.pullStagedDelta() } else await controller.pullRegisteredDelta(targetId) }
    async function startBidi(): Promise<void> { if (targetId === 'new-link') { controller.stageLink(stagedUri); await controller.syncStagedBidirectional() } else await controller.syncRegisteredBidirectional(targetId) }
    async function resolve(winner: 'local' | 'remote'): Promise<void> { if (selectedIncoming) await controller.resolveRegisteredBidirectional(selectedIncoming.deviceId, winner) }
    async function abandon(): Promise<void> { if (await alertConfirm(sync.work.abandonConfirm)) await controller.abandonBidirectional() }
    function bytes(value?: number): string { return `${value ?? 0}` }
    function time(value?: number): string { return value ? new Date(value).toLocaleString() : '-' }
    function format(template: string, ...values: Array<string | number>): string {
        return values.reduce<string>((value, next, index) => value.replace(`{${index}}`, String(next)), template)
    }
    onMount(() => {
        const unsubscribe = controller.subscribe((next) => { snapshot = next; void createQr() })
        void controller.initialize()
        const timer = setInterval(() => { now = Date.now() }, 1000)
        return () => { clearInterval(timer); unsubscribe() }
    })
</script>

<section class="mt-4">
    <h3 class="text-xl font-bold">{sync.menuTitle}</h3>
    <p class="mt-1 text-sm text-textcolor2">{sync.intro}</p>
    {#if isTauriAndroid}<p class="mt-3 rounded-md border border-borderc bg-bgcolor p-2 text-sm text-textcolor2">{sync.androidLanOnly}</p>{/if}

    <div class="mt-4 rounded-md border border-darkborderc bg-darkbg p-4">
        <div class="flex items-center justify-between gap-3"><h4 class="font-bold">{sync.share.title}</h4><span class="flex items-center gap-2 text-sm"><span class="h-2 w-2 rounded-full {stateColor()}"></span>{stateLabel()}</span></div>
        {#if !isTauriAndroid}<div class="mt-3 flex flex-wrap gap-2" aria-label={sync.share.title}>{#each [['lan', sync.share.methodLan], ['quick', sync.share.methodQuick], ['fixed-url', sync.share.methodFixed]] as [method, label]}<Button size="sm" styled="outlined" selected={settings.syncListenMethod === method} onclick={() => updateSettings({ syncListenMethod: method as typeof settings.syncListenMethod })}>{label}</Button>{/each}</div>{/if}
        {#if settings.syncListenMethod !== 'quick' || isTauriAndroid}<label class="mt-3 block text-sm font-bold" for="device-sync-port">{sync.share.port}</label><input id="device-sync-port" type="number" min="1" max="65535" class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={settings.syncFixedPort} oninput={(event) => updateSettings({ syncFixedPort: Number(event.currentTarget.value) })} />{:else}<p class="mt-3 text-sm text-textcolor2">{sync.share.quickNote}</p>{/if}
        {#if settings.syncListenMethod === 'fixed-url' && !isTauriAndroid}<label class="mt-3 block text-sm font-bold" for="device-sync-public-url">{sync.share.publicUrl}</label><input id="device-sync-public-url" type="url" class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={settings.syncPublicBaseUrl} oninput={(event) => updateSettings({ syncPublicBaseUrl: event.currentTarget.value })} /><p class="mt-2 text-sm text-textcolor2">{sync.share.fixedNote}</p><details class="mt-2 text-sm text-textcolor2"><summary>{sync.share.fixedGuideTitle}</summary><p class="mt-2">{format(sync.share.fixedGuideBody, settings.syncFixedPort)}</p></details>{/if}
        <label class="mt-3 flex gap-2 text-sm"><input type="checkbox" checked={settings.syncAutoListen} onchange={(event) => updateSettings({ syncAutoListen: event.currentTarget.checked })} />{sync.share.autoListen}</label>
        <Button className="mt-3" styled={snapshot.source.phase === 'running' ? 'danger' : 'primary'} onclick={startOrStop}>{snapshot.source.phase === 'running' ? sync.share.stop : sync.share.start}</Button>
        {#if snapshot.source.phase === 'running' && pairUri}<div class="mt-4 border-t border-darkborderc pt-4"><label class="flex gap-2 text-sm"><input type="checkbox" bind:checked={permissions.read} />{sync.share.permRead}</label><label class="mt-2 flex gap-2 text-sm"><input type="checkbox" bind:checked={permissions.bidirectional} />{sync.share.permBidirectional}</label><p class="mt-1 text-xs text-textcolor2">{sync.share.permBidirectionalHelp}</p><div class="mt-3 flex flex-wrap gap-4">{#if qrDataUrl}<img class:opacity-40={expired} src={qrDataUrl} alt={sync.share.pairTitle} width="192" height="192" />{/if}<div><h5 class="font-bold">{sync.share.pairTitle}</h5><p class="mt-1 text-sm text-textcolor2">{sync.share.pairNote}</p><p class="mt-2 text-sm">{expired ? sync.share.pairExpired : format(sync.share.pairRemaining, `${Math.ceil(remaining / 1000)}s`)}</p><div class="mt-2 flex gap-2"><Button size="sm" disabled={expired} onclick={copyLink}>{sync.share.copyLink}</Button><Button size="sm" styled="outlined" onclick={rotate}>{sync.share.newLink}</Button></div></div></div></div>{/if}
        {#if sourceError}<p class="mt-3 text-sm text-draculared">{sourceError === 'registration-expired' ? sync.registrationExpired : sync.share.stateError}</p>{/if}
    </div>

    <div class="mt-4 rounded-md border border-darkborderc bg-darkbg p-4"><h4 class="font-bold">{sync.devices.title}</h4><p class="mt-1 text-sm text-textcolor2">{sync.devices.note}</p>
        <div class="mt-4"><h5 class="font-bold">{sync.devices.outgoingTitle}</h5>{#if snapshot.devices.length === 0}<p class="mt-2 text-sm text-textcolor2">{sync.devices.outgoingEmpty}</p>{:else}{#each snapshot.devices as device (device.deviceId)}<div class="mt-2 flex items-center justify-between border-t border-darkborderc pt-2 text-sm"><div><p>{device.name || device.deviceId.slice(0, 8)}</p><p class="text-textcolor2">{format(sync.devices.lastSeen, time(device.lastSeenMs), bytes(device.totalBytes))}</p><p class="text-xs text-textcolor2">{device.permissions.includes('bidirectional') ? sync.devices.permBidirectional : sync.devices.permRead}</p></div><Button size="sm" styled="danger" onclick={() => revoke('outgoing', device.deviceId)}>{sync.devices.revoke}</Button></div>{/each}{/if}</div>
        <div class="mt-4"><h5 class="font-bold">{sync.devices.incomingTitle}</h5>{#if snapshot.sources.length === 0}<p class="mt-2 text-sm text-textcolor2">{sync.devices.incomingEmpty}</p>{:else}{#each snapshot.sources as device (device.deviceId)}<div class="mt-2 flex items-center justify-between border-t border-darkborderc pt-2 text-sm"><div><p>{device.name || device.deviceId.slice(0, 8)}</p><p class="text-textcolor2">{format(sync.devices.lastSeen, time(device.lastSeenMs), bytes(device.totalBytes))}</p><p class="text-xs text-textcolor2">{device.permissions.includes('bidirectional') ? sync.devices.permBidirectional : sync.devices.permRead}</p></div><Button size="sm" styled="danger" onclick={() => revoke('incoming', device.deviceId)}>{sync.devices.revoke}</Button></div>{/each}{/if}</div>
    </div>

    <div class="mt-4 rounded-md border border-darkborderc bg-darkbg p-4"><h4 class="font-bold">{sync.work.title}</h4><label class="mt-3 block text-sm font-bold" for="device-sync-target">{sync.work.target}</label><select id="device-sync-target" class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={targetId} onchange={(event) => chooseTarget(event.currentTarget.value)}><option value="new-link">{sync.work.useNewLink}</option>{#each snapshot.sources as source (source.deviceId)}<option value={source.deviceId}>{source.name || source.deviceId.slice(0, 8)}</option>{/each}</select>{#if targetId === 'new-link'}<label class="sr-only" for="device-sync-link">{sync.work.linkPlaceholder}</label><input id="device-sync-link" class="mt-2 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" placeholder={sync.work.linkPlaceholder} bind:value={stagedUri} />{/if}<div class="mt-3 flex flex-wrap gap-2"><Button disabled={sharing} onclick={startClone}>{sync.work.clone}</Button><Button disabled={sharing} onclick={startDelta}>{sync.work.delta}</Button><Button disabled={sharing} onclick={startBidi}>{sync.work.bidirectional}</Button></div>{#if sharing}<p class="mt-2 text-sm text-textcolor2">{sync.work.blockedWhileSharing}</p>{/if}
        {#if cloneTarget?.phase === 'downloading'}<progress class="mt-3 w-full" value={cloneTarget.completedBytes} max={cloneTarget.totalBytes ?? Math.max(1, cloneTarget.completedBytes)}></progress><p class="mt-1 text-sm">{format(sync.work.progress, Math.round((cloneTarget.completedBytes / Math.max(1, cloneTarget.totalBytes ?? cloneTarget.completedBytes)) * 100))}</p><Button size="sm" styled="danger" onclick={() => controller.cancelClone()}>{sync.work.cancel}</Button>{:else if cloneTarget?.phase === 'cancelled' || cloneTarget?.phase === 'failed'}<Button className="mt-3" size="sm" onclick={() => controller.resumeClone()}>{sync.work.resume}</Button>{:else if delta?.pullPhase === 'completed'}<p class="mt-3 text-sm">{sync.work.doneUpToDate}</p>{:else if delta?.pullPhase === 'fullCloneRequired'}<p class="mt-3 text-sm text-draculared">{sync.work.needClone}</p>{:else if bidiPhase === 'awaitingConflict'}<div class="mt-3 rounded-md border border-draculared p-3"><p class="font-bold text-draculared">{sync.work.conflictTitle}</p><p class="mt-1 text-sm text-textcolor2">{sync.work.conflictBody}</p><p class="mt-2 text-sm">{format(sync.work.conflictOthers, 1)}</p><div class="mt-2 flex gap-2"><Button size="sm" onclick={() => resolve('local')}>{sync.work.keepThis}</Button><Button size="sm" onclick={() => resolve('remote')}>{sync.work.keepOther}</Button></div></div>{:else if ['sourceUnavailable', 'refreshPending', 'localCommitted', 'targetPrepared'].includes(bidiPhase)}<div class="mt-3"><Button size="sm" onclick={() => controller.resumeBidirectional()}>{sync.work.resume}</Button><Button className="ml-2" size="sm" styled="danger" onclick={abandon}>{sync.work.abandon}</Button></div>{:else if bidiPhase === 'sourcePrepared'}<div class="mt-3"><p class="text-sm text-textcolor2">{sync.work.resume}</p><Button size="sm" styled="danger" onclick={abandon}>{sync.work.abandon}</Button></div>{:else if bidiPhase === 'completed' || bidiPhase === 'stale'}<div class="mt-3"><p class="text-sm">{sync.work.doneUpToDate}</p><Button size="sm" onclick={() => controller.acknowledgeBidirectional()}>{sync.work.dismiss}</Button></div>{/if}{#if cloneTarget?.phase === 'completed' || bidiPhase === 'completed'}<p class="mt-3 text-sm text-textcolor2">{sync.work.backupNote}</p>{/if}</div>
    <p class="mt-4 rounded-md border border-yellow-600 bg-bgcolor p-3 text-sm text-textcolor2">{sync.lanWarning}</p>
</section>
