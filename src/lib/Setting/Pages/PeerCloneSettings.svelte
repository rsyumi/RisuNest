<script lang="ts">
    import { onMount } from 'svelte'

    import { language } from 'src/lang'
    import { alertConfirm } from 'src/ts/alert'
    import {
        initialPeerCloneState,
        pairingUriForQr,
        type PeerCloneNativeCapabilities,
        type PeerCloneSourceStatus,
        type PeerCloneState,
    } from 'src/ts/storage/sync/peerClone'
    import { getDesktopPeerCloneController } from 'src/ts/storage/sync/peerCloneController'
    import {
        consumePendingPeerCloneUri,
        subscribePeerCloneUri,
    } from 'src/ts/storage/sync/peerCloneDeepLink'
    import {
        acquireDestructiveReplacementFence,
        capturePersistentMutationToken,
        flushPendingData,
    } from 'src/ts/storage/persistentDataRuntime.svelte'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import SelectInput from 'src/lib/UI/GUI/SelectInput.svelte'
    const controller = getDesktopPeerCloneController({
        flushPendingData,
        capturePersistentMutationToken,
        acquireDestructiveReplacementFence,
    })
    let capabilities = $state<PeerCloneNativeCapabilities>()
    let sourceStatus = $state<PeerCloneSourceStatus>({ phase: 'idle', devices: [] })
    let cloneState = $state<PeerCloneState>(initialPeerCloneState)
    let pairingInput = $state('')
    let sourcePairingUri = $state('')
    let qrDataUrl = $state('')
    let busy = $state(false)
    let error = $state('')
    let sourceMode = $state<'lan' | 'quick' | 'named'>('lan')
    let namedTunnelToken = $state('')
    let namedTunnelPublicBaseUrl = $state('')

    const sourceEnabled = $derived(!!(
        capabilities?.productionEnabled
        && capabilities.sourceReady
        && capabilities.losslessBackupReady
        && capabilities.httpTransportReady
    ))
    const targetEnabled = $derived(!!(
        capabilities?.productionEnabled
        && capabilities.atomicActivationReady
        && capabilities.losslessBackupReady
        && capabilities.httpTransportReady
    ))
    const progressMaximum = $derived(cloneState.target.totalBytes ?? Math.max(1, cloneState.target.completedBytes))

    function reportError(cause: unknown): void {
        error = cause instanceof Error ? cause.message : String(cause)
    }

    function refreshState(): void {
        const snapshot = controller.snapshot()
        cloneState = snapshot.state
        sourceStatus = snapshot.sourceStatus
        capabilities = snapshot.capabilities
        sourcePairingUri = snapshot.sourcePairingUri
        error = snapshot.error || snapshot.warning
    }

    function acceptPairingUri(uri: string): void {
        pairingInput = uri
        error = ''
        try {
            controller.join(uri)
            refreshState()
        } catch {
            error = language.peerClone.invalidLink
        }
    }

    async function withBusy(operation: () => Promise<void>): Promise<void> {
        if (busy) return
        busy = true
        error = ''
        try {
            await operation()
        } catch (cause) {
            reportError(cause)
        } finally {
            busy = false
            refreshState()
        }
    }

    async function prepareSource(): Promise<void> {
        await withBusy(async () => {
            sourceStatus = await controller.prepare()
        })
    }

    async function startSource(): Promise<void> {
        if (!sourceStatus.sessionId) return
        if (sourceMode === 'quick' && !await alertConfirm(language.peerClone.quickTunnelConfirm)) return
        const sessionId = sourceStatus.sessionId
        const token = namedTunnelToken
        if (sourceMode === 'named') namedTunnelToken = ''
        try {
            await withBusy(async () => {
                if (sourceMode === 'quick') {
                    sourceStatus = await controller.startQuickTunnel(sessionId)
                } else if (sourceMode === 'named') {
                    sourceStatus = await controller.startNamedTunnel(sessionId, token, namedTunnelPublicBaseUrl)
                } else {
                    sourceStatus = await controller.start(sessionId)
                }
                sourcePairingUri = sourceStatus.pairingUri ?? ''
                await updateSourceQr(sourcePairingUri)
            })
        } finally {
            namedTunnelToken = ''
        }
    }

    async function updateSourceQr(pairingUri: string): Promise<void> {
        if (!pairingUri) return
        const { default: QRCode } = await import('qrcode')
        qrDataUrl = await QRCode.toDataURL(pairingUriForQr(pairingUri), { margin: 1, width: 256 })
    }

    async function restoreSourceQr(): Promise<void> {
        const pairingUri = controller.snapshot().sourcePairingUri
        if (!pairingUri || qrDataUrl) return
        await updateSourceQr(pairingUri)
    }

    async function stopSource(): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => {
            await controller.stop(sourceStatus.sessionId!)
            refreshState()
            sourcePairingUri = ''
            qrDataUrl = ''
        })
    }

    async function revokeDevice(deviceId: string): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => {
            await controller.revoke(sourceStatus.sessionId!, deviceId)
            refreshState()
        })
    }

    async function downloadClone(): Promise<void> {
        if (!await alertConfirm(language.peerClone.replacementConfirm)) return
        controller.confirmDestructiveReplace()
        await withBusy(async () => {
            await controller.download()
        })
    }

    async function resumeClone(): Promise<void> {
        await withBusy(async () => {
            await controller.resume()
        })
    }

    async function cancelClone(): Promise<void> {
        await withBusy(async () => {
            await controller.cancel()
        })
    }

    onMount(() => {
        const unsubscribeController = controller.subscribe(() => refreshState())
        void controller.initialize()
        void restoreSourceQr().catch(reportError)
        const pendingUri = consumePendingPeerCloneUri()
        if (pendingUri) acceptPairingUri(pendingUri)
        const unsubscribe = subscribePeerCloneUri(acceptPairingUri)
        return () => {
            unsubscribe()
            unsubscribeController()
        }
    })
</script>

<section class="mt-4 rounded-md border border-darkborderc bg-darkbg p-3">
    <h3 class="text-xl font-bold">{language.peerClone.title}</h3>
    <p class="mt-1 text-sm text-textcolor2">{language.peerClone.description}</p>

    {#if capabilities && (!sourceEnabled || !targetEnabled)}
        <p class="mt-3 rounded-md border border-borderc bg-bgcolor p-2 text-sm text-textcolor2">
            {language.peerClone.unavailable}
        </p>
    {/if}

    <div class="mt-4 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerClone.source}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerClone.sourceHelp}</p>
        {#if sourceStatus.phase === 'prepared'}
            <p class="mt-3 text-sm font-bold">{language.peerClone.shareMode}</p>
            <SelectInput bind:value={sourceMode} className="mt-1 w-full">
                <option value="lan">{language.peerClone.lanMode}</option>
                <option value="quick">{language.peerClone.quickTunnelMode}</option>
                <option value="named">{language.peerClone.namedTunnelMode}</option>
            </SelectInput>
            {#if sourceMode === 'quick'}
                <p class="mt-2 rounded-md border border-borderc bg-bgcolor p-2 text-sm text-textcolor2">
                    {language.peerClone.quickTunnelHelp}
                </p>
            {:else if sourceMode === 'named'}
                <label class="mt-3 block text-sm font-bold" for="peer-clone-named-token">{language.peerClone.namedTunnelToken}</label>
                <input
                    id="peer-clone-named-token"
                    bind:value={namedTunnelToken}
                    type="password"
                    autocomplete="new-password"
                    class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
                />
                <label class="mt-3 block text-sm font-bold" for="peer-clone-named-public-url">{language.peerClone.namedTunnelPublicUrl}</label>
                <input
                    id="peer-clone-named-public-url"
                    bind:value={namedTunnelPublicBaseUrl}
                    type="url"
                    autocomplete="off"
                    placeholder="https://sync.example.com"
                    class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
                />
                <p class="mt-2 text-sm text-textcolor2">{language.peerClone.namedTunnelHelp}</p>
            {/if}
        {/if}
        <div class="mt-2 flex flex-wrap gap-2">
            <Button disabled={!sourceEnabled || busy} onclick={prepareSource}>{language.peerClone.prepare}</Button>
            <Button
                disabled={!sourceEnabled || busy || sourceStatus.phase !== 'prepared'}
                onclick={startSource}
            >{language.peerClone.start}</Button>
            <Button
                styled="danger"
                disabled={busy || !['prepared', 'starting', 'running', 'stopping'].includes(sourceStatus.phase)}
                onclick={stopSource}
            >{language.peerClone.stop}</Button>
        </div>

        {#if sourceStatus.tunnel && sourceStatus.phase === 'stopping'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerClone.tunnelCleanup}</p>
        {/if}

        {#if sourcePairingUri}
            <label class="mt-3 block text-sm font-bold" for="peer-clone-source-uri">{language.peerClone.pairingLink}</label>
            <textarea
                id="peer-clone-source-uri"
                readonly
                rows="3"
                class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
                value={sourcePairingUri}
            ></textarea>
            <Button className="mt-2" onclick={() => navigator.clipboard.writeText(sourcePairingUri)}>
                {language.peerClone.copyLink}
            </Button>
            {#if qrDataUrl}
                <img class="mt-2 h-64 w-64 bg-white p-2" src={qrDataUrl} alt={language.peerClone.pairingLink} />
            {/if}
        {/if}

        {#if sourceStatus.devices.length > 0}
            <h5 class="mt-3 font-bold">{language.peerClone.devices}</h5>
            <ul class="mt-1 flex flex-col gap-2">
                {#each sourceStatus.devices as device (device.deviceId)}
                    <li class="flex items-center justify-between gap-2 rounded-md bg-bgcolor p-2 text-sm">
                        <span>{device.deviceId} ({device.verifiedBytes.toLocaleString()} bytes)</span>
                        <Button
                            size="sm"
                            styled="danger"
                            disabled={busy || device.revoked}
                            onclick={() => revokeDevice(device.deviceId)}
                        >{language.peerClone.revoke}</Button>
                    </li>
                {/each}
            </ul>
        {/if}
    </div>

    <div class="mt-3 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerClone.target}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerClone.targetHelp}</p>
        <label class="mt-2 block text-sm font-bold" for="peer-clone-target-uri">{language.peerClone.pairingLink}</label>
        <textarea
            id="peer-clone-target-uri"
            bind:value={pairingInput}
            rows="3"
            class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
        ></textarea>
        <div class="mt-2 flex flex-wrap gap-2">
            <Button disabled={busy || !pairingInput} onclick={() => acceptPairingUri(pairingInput)}>
                {language.peerClone.useLink}
            </Button>
            <Button
                disabled={!targetEnabled || busy || cloneState.target.phase !== 'joined'}
                onclick={downloadClone}
            >{language.peerClone.download}</Button>
            <Button
                disabled={!targetEnabled || busy || (cloneState.target.phase !== 'cancelled' && cloneState.target.phase !== 'failed')}
                onclick={resumeClone}
            >{language.peerClone.resume}</Button>
            <Button
                styled="danger"
                disabled={busy || cloneState.target.phase !== 'downloading'}
                onclick={cancelClone}
            >{language.peerClone.cancel}</Button>
        </div>

        {#if cloneState.target.phase === 'downloading' || cloneState.target.phase === 'cancelled' || cloneState.target.phase === 'completed'}
            <label class="mt-3 block text-sm font-bold" for="peer-clone-progress">{language.peerClone.progress}</label>
            <progress
                id="peer-clone-progress"
                class="mt-1 w-full"
                value={cloneState.target.completedBytes}
                max={progressMaximum}
            ></progress>
            <p class="text-sm text-textcolor2">
                {cloneState.target.completedBytes.toLocaleString()} / {cloneState.target.totalBytes?.toLocaleString() ?? '?'} bytes
            </p>
        {/if}
    </div>

    {#if error}
        <p class="mt-3 text-sm text-draculared">{error}</p>
    {/if}
</section>
