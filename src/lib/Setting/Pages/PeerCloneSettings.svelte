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
        type PeerDeltaCapabilities,
        type PeerDeltaPullResult,
        type PeerDeltaSourceStatus,
        parsePeerDeltaUri,
    } from 'src/ts/storage/sync/peerDelta'
    import { getDesktopPeerDeltaController } from 'src/ts/storage/sync/peerDeltaController'
    import {
        parsePeerBidirectionalUri,
        type PeerBidirectionalCapabilities,
        type PeerBidirectionalSourceStatus,
        type PeerBidirectionalSyncResult,
    } from 'src/ts/storage/sync/peerBidirectional'
    import { getDesktopPeerBidirectionalController } from 'src/ts/storage/sync/peerBidirectionalController'
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
    const deltaController = getDesktopPeerDeltaController({
        flushPendingData,
        capturePersistentMutationToken,
        acquirePersistentMutationFence: acquireDestructiveReplacementFence,
    })
    const bidirectionalController = getDesktopPeerBidirectionalController({
        flushPendingData,
        capturePersistentMutationToken,
        acquirePersistentMutationFence: acquireDestructiveReplacementFence,
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
    let deltaCapabilities = $state<PeerDeltaCapabilities>()
    let deltaSourceStatus = $state<PeerDeltaSourceStatus>({ phase: 'idle', devices: [] })
    let deltaSourcePairingUri = $state('')
    let deltaPairingInput = $state('')
    let deltaPullPhase = $state<'idle' | 'running' | 'completed' | 'fullCloneRequired' | 'conflict' | 'failed'>('idle')
    let deltaPullResult = $state<PeerDeltaPullResult>()
    let deltaBusy = $state(false)
    let deltaError = $state('')
    let bidirectionalCapabilities = $state<PeerBidirectionalCapabilities>()
    let bidirectionalSourceStatus = $state<PeerBidirectionalSourceStatus>({ phase: 'idle', devices: [] })
    let bidirectionalSourcePairingUri = $state('')
    let bidirectionalPairingInput = $state('')
    let bidirectionalOperationPhase = $state('idle')
    let bidirectionalResult = $state<PeerBidirectionalSyncResult>()
    let bidirectionalOperationRetained = $state(false)
    let bidirectionalBusy = $state(false)
    let bidirectionalSourceBusy = $state(false)
    let bidirectionalSourceError = $state('')
    let bidirectionalOperationError = $state('')
    let bidirectionalInputError = $state('')

    $effect(() => {
        if (sourceMode !== 'named' || sourceStatus.phase !== 'prepared') {
            namedTunnelToken = ''
        }
    })

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
    const deltaSourceEnabled = $derived(!!(
        deltaCapabilities?.productionEnabled
        && deltaCapabilities.sourceReady
        && deltaCapabilities.authenticatedTransportReady
    ))
    const deltaTargetEnabled = $derived(!!(
        deltaCapabilities?.productionEnabled
        && deltaCapabilities.atomicActivationReady
        && deltaCapabilities.authenticatedTransportReady
    ))
    const deltaOperationRunning = $derived(deltaBusy || deltaPullPhase === 'running')
    const bidirectionalEnabled = $derived(!!(
        bidirectionalCapabilities?.productionEnabled
        && bidirectionalCapabilities.sourceReady
        && bidirectionalCapabilities.atomicActivationReady
        && bidirectionalCapabilities.authenticatedTransportReady
        && bidirectionalCapabilities.losslessBackupReady
        && bidirectionalCapabilities.durableStateReady
    ))
    const bidirectionalBackups = $derived(
        bidirectionalResult && 'backups' in bidirectionalResult ? bidirectionalResult.backups : [],
    )
    const bidirectionalControlBusy = $derived(bidirectionalBusy || bidirectionalSourceBusy)

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

    function refreshDeltaState(): void {
        const snapshot = deltaController.snapshot()
        deltaCapabilities = snapshot.capabilities
        deltaSourceStatus = snapshot.sourceStatus
        deltaSourcePairingUri = snapshot.sourcePairingUri
        deltaPullPhase = snapshot.pullPhase
        deltaPullResult = snapshot.pullResult
        deltaError = snapshot.error
    }

    async function withDeltaBusy(operation: () => Promise<void>): Promise<void> {
        if (deltaBusy) return
        deltaBusy = true
        deltaError = ''
        try {
            await operation()
        } catch (cause) {
            deltaError = cause instanceof Error ? cause.message : String(cause)
        } finally {
            deltaBusy = false
            refreshDeltaState()
        }
    }

    async function prepareDeltaSource(): Promise<void> {
        await withDeltaBusy(async () => {
            await deltaController.prepare()
        })
    }

    async function startDeltaSource(): Promise<void> {
        if (!deltaSourceStatus.sessionId) return
        await withDeltaBusy(async () => {
            await deltaController.start(deltaSourceStatus.sessionId!)
        })
    }

    async function stopDeltaSource(): Promise<void> {
        if (!deltaSourceStatus.sessionId) return
        await withDeltaBusy(async () => {
            await deltaController.stop(deltaSourceStatus.sessionId!)
        })
    }

    async function revokeDeltaDevice(deviceId: string): Promise<void> {
        if (!deltaSourceStatus.sessionId) return
        await withDeltaBusy(async () => {
            await deltaController.revoke(deltaSourceStatus.sessionId!, deviceId)
        })
    }

    async function pullDelta(): Promise<void> {
        if (!deltaPairingInput) return
        try {
            parsePeerDeltaUri(deltaPairingInput)
        } catch {
            deltaError = language.peerDelta.invalidLink
            return
        }
        await withDeltaBusy(async () => {
            await deltaController.pull(deltaPairingInput)
        })
    }

    function deltaSourcePhaseText(): string {
        switch (deltaSourceStatus.phase) {
            case 'prepared': return language.peerDelta.statusPrepared
            case 'running': return language.peerDelta.statusRunning
            case 'stopped': return language.peerDelta.statusStopped
            default: return language.peerDelta.statusIdle
        }
    }

    function refreshBidirectionalState(): void {
        const snapshot = bidirectionalController.snapshot()
        bidirectionalCapabilities = snapshot.capabilities
        bidirectionalSourceStatus = snapshot.sourceStatus
        bidirectionalSourcePairingUri = snapshot.sourcePairingUri
        bidirectionalOperationPhase = snapshot.operationPhase
        bidirectionalResult = snapshot.operationResult
        bidirectionalOperationRetained = snapshot.operationRetained
        bidirectionalSourceBusy = snapshot.sourceBusy
        bidirectionalSourceError = snapshot.sourceError
        bidirectionalOperationError = snapshot.operationError
    }

    async function withBidirectionalBusy(operation: () => Promise<void>): Promise<void> {
        if (bidirectionalBusy) return
        bidirectionalBusy = true
        try {
            await operation()
        } catch {
            // The controller retains source and target errors separately.
        } finally {
            bidirectionalBusy = false
            refreshBidirectionalState()
        }
    }

    async function prepareBidirectionalSource(): Promise<void> {
        await withBidirectionalBusy(async () => {
            await bidirectionalController.prepare()
        })
    }

    async function startBidirectionalSource(): Promise<void> {
        if (!bidirectionalSourceStatus.sessionId) return
        await withBidirectionalBusy(async () => {
            await bidirectionalController.start(bidirectionalSourceStatus.sessionId!)
        })
    }

    async function stopBidirectionalSource(): Promise<void> {
        if (!bidirectionalSourceStatus.sessionId) return
        await withBidirectionalBusy(async () => {
            await bidirectionalController.stop(bidirectionalSourceStatus.sessionId!)
        })
    }

    async function revokeBidirectionalDevice(deviceId: string): Promise<void> {
        const sessionId = bidirectionalSourceStatus.sessionId ?? ''
        await withBidirectionalBusy(async () => {
            await bidirectionalController.revoke(sessionId, deviceId)
        })
    }

    async function syncBidirectional(): Promise<void> {
        if (!bidirectionalPairingInput) return
        try {
            parsePeerBidirectionalUri(bidirectionalPairingInput)
        } catch {
            bidirectionalInputError = language.peerBidirectional.invalidLink
            return
        }
        bidirectionalInputError = ''
        await withBidirectionalBusy(async () => {
            await bidirectionalController.sync(bidirectionalPairingInput)
        })
    }

    async function resolveBidirectional(winner: 'local' | 'remote'): Promise<void> {
        await withBidirectionalBusy(async () => {
            await bidirectionalController.resolve(winner)
        })
    }

    async function resumeBidirectional(): Promise<void> {
        await withBidirectionalBusy(async () => {
            await bidirectionalController.resume()
        })
    }

    async function acknowledgeBidirectional(): Promise<void> {
        await withBidirectionalBusy(async () => {
            await bidirectionalController.acknowledge()
        })
    }

    async function abandonBidirectional(): Promise<void> {
        if (!await alertConfirm(language.peerBidirectional.abandonConfirm)) return
        await withBidirectionalBusy(async () => {
            await bidirectionalController.abandon()
        })
    }

    onMount(() => {
        const unsubscribeController = controller.subscribe(() => refreshState())
        const unsubscribeDeltaController = deltaController.subscribe(() => refreshDeltaState())
        const unsubscribeBidirectionalController = bidirectionalController.subscribe(() => refreshBidirectionalState())
        void controller.initialize()
        void deltaController.initialize()
        void bidirectionalController.initialize()
        void restoreSourceQr().catch(reportError)
        const pendingUri = consumePendingPeerCloneUri()
        if (pendingUri) acceptPairingUri(pendingUri)
        const unsubscribe = subscribePeerCloneUri(acceptPairingUri)
        return () => {
            unsubscribe()
            unsubscribeController()
            unsubscribeDeltaController()
            unsubscribeBidirectionalController()
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

<section class="mt-4 rounded-md border border-darkborderc bg-darkbg p-3">
    <h3 class="text-xl font-bold">{language.peerDelta.title}</h3>
    <p class="mt-1 text-sm text-textcolor2">{language.peerDelta.description}</p>

    {#if deltaCapabilities && (!deltaSourceEnabled || !deltaTargetEnabled)}
        <p class="mt-3 rounded-md border border-borderc bg-bgcolor p-2 text-sm text-textcolor2">
            {language.peerDelta.unavailable}
        </p>
    {/if}

    <div class="mt-4 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerDelta.source}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerDelta.sourceHelp}</p>
        <p class="mt-1 text-sm text-textcolor2">{language.peerDelta.sourceOpenHelp}</p>
        <p class="mt-2 text-sm">
            <span class="font-bold">{language.peerDelta.sourceStatus}:</span>
            {deltaSourcePhaseText()}
        </p>
        <div class="mt-2 flex flex-wrap gap-2">
            <Button disabled={!deltaSourceEnabled || deltaOperationRunning} onclick={prepareDeltaSource}>
                {language.peerDelta.prepare}
            </Button>
            <Button
                disabled={!deltaSourceEnabled || deltaOperationRunning || deltaSourceStatus.phase !== 'prepared'}
                onclick={startDeltaSource}
            >{language.peerDelta.start}</Button>
            <Button
                styled="danger"
                disabled={deltaOperationRunning || !['prepared', 'running'].includes(deltaSourceStatus.phase)}
                onclick={stopDeltaSource}
            >{language.peerDelta.stop}</Button>
        </div>

        {#if deltaSourcePairingUri}
            <label class="mt-3 block text-sm font-bold" for="peer-delta-source-uri">{language.peerDelta.pairingLink}</label>
            <textarea
                id="peer-delta-source-uri"
                readonly
                rows="3"
                class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
                value={deltaSourcePairingUri}
            ></textarea>
            <Button className="mt-2" disabled={deltaOperationRunning} onclick={() => navigator.clipboard.writeText(deltaSourcePairingUri)}>
                {language.peerDelta.copyLink}
            </Button>
        {/if}

        {#if deltaSourceStatus.devices.length > 0}
            <h5 class="mt-3 font-bold">{language.peerDelta.devices}</h5>
            <ul class="mt-1 flex flex-col gap-2">
                {#each deltaSourceStatus.devices as device (device.deviceId)}
                    <li class="flex items-center justify-between gap-2 rounded-md bg-bgcolor p-2 text-sm">
                        <span>{device.deviceId} ({device.transferredBytes.toLocaleString()} bytes)</span>
                        <Button
                            size="sm"
                            styled="danger"
                            disabled={deltaOperationRunning || device.revoked}
                            onclick={() => revokeDeltaDevice(device.deviceId)}
                        >{language.peerDelta.revoke}</Button>
                    </li>
                {/each}
            </ul>
        {/if}
    </div>

    <div class="mt-3 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerDelta.target}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerDelta.targetHelp}</p>
        <p class="mt-1 rounded-md border border-borderc bg-bgcolor p-2 text-sm text-textcolor2">
            {language.peerDelta.divergenceHelp}
        </p>
        <label class="mt-3 block text-sm font-bold" for="peer-delta-target-uri">{language.peerDelta.pairingLink}</label>
        <textarea
            id="peer-delta-target-uri"
            bind:value={deltaPairingInput}
            rows="3"
            class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
        ></textarea>
        <Button
            className="mt-2"
            disabled={!deltaTargetEnabled || deltaOperationRunning || !deltaPairingInput}
            onclick={pullDelta}
        >{language.peerDelta.pull}</Button>

        {#if deltaPullPhase === 'running'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerDelta.pulling}</p>
        {:else if deltaPullResult?.kind === 'noChanges'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerDelta.noChanges}</p>
        {:else if deltaPullResult?.kind === 'updated'}
            <p class="mt-2 text-sm text-textcolor2">
                {language.peerDelta.updated(
                    deltaPullResult.transferredObjects,
                    deltaPullResult.transferredBytes.toLocaleString(),
                )}
            </p>
        {:else if deltaPullResult?.kind === 'fullCloneRequired'}
            <p class="mt-2 text-sm text-draculared">{language.peerDelta.fullCloneRequired}</p>
        {:else if deltaPullResult?.kind === 'conflict' && deltaPullResult.reason === 'localAndRemoteChanged'}
            <p class="mt-2 text-sm text-draculared">{language.peerDelta.conflictLocalRemote}</p>
        {:else if deltaPullResult?.kind === 'conflict'}
            <p class="mt-2 text-sm text-draculared">{language.peerDelta.conflictStaleRevision}</p>
        {/if}
    </div>

    {#if deltaError}
        <p class="mt-3 text-sm text-draculared">{deltaError}</p>
    {/if}
</section>

<section class="mt-4 rounded-md border border-darkborderc bg-darkbg p-3">
    <h3 class="text-xl font-bold">{language.peerBidirectional.title}</h3>
    <p class="mt-1 text-sm text-textcolor2">{language.peerBidirectional.description}</p>

    {#if bidirectionalCapabilities && !bidirectionalEnabled}
        <p class="mt-3 rounded-md border border-borderc bg-bgcolor p-2 text-sm text-textcolor2">
            {language.peerBidirectional.unavailable}
        </p>
    {/if}

    <div class="mt-4 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerBidirectional.source}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerBidirectional.sourceHelp}</p>
        <div class="mt-2 flex flex-wrap gap-2">
            <Button
                disabled={!bidirectionalEnabled
                    || bidirectionalControlBusy
                    || (bidirectionalOperationRetained
                        && !['completed', 'sourcePrepared'].includes(bidirectionalOperationPhase))}
                onclick={prepareBidirectionalSource}
            >
                {language.peerBidirectional.prepare}
            </Button>
            <Button
                disabled={!bidirectionalEnabled
                    || bidirectionalControlBusy
                    || (bidirectionalOperationRetained
                        && !['completed', 'sourcePrepared'].includes(bidirectionalOperationPhase))
                    || bidirectionalSourceStatus.phase !== 'prepared'}
                onclick={startBidirectionalSource}
            >{language.peerBidirectional.start}</Button>
            <Button
                styled="danger"
                disabled={bidirectionalControlBusy || !['prepared', 'running'].includes(bidirectionalSourceStatus.phase)}
                onclick={stopBidirectionalSource}
            >{language.peerBidirectional.stop}</Button>
        </div>

        {#if bidirectionalSourcePairingUri}
            <label class="mt-3 block text-sm font-bold" for="peer-bidirectional-source-uri">
                {language.peerBidirectional.pairingLink}
            </label>
            <textarea
                id="peer-bidirectional-source-uri"
                readonly
                rows="3"
                class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
                value={bidirectionalSourcePairingUri}
            ></textarea>
            <Button
                className="mt-2"
                disabled={bidirectionalControlBusy}
                onclick={() => navigator.clipboard.writeText(bidirectionalSourcePairingUri)}
            >{language.peerBidirectional.copyLink}</Button>
        {/if}

        {#if bidirectionalSourceStatus.devices.length > 0}
            <h5 class="mt-3 font-bold">{language.peerBidirectional.devices}</h5>
            <ul class="mt-1 flex flex-col gap-2">
                {#each bidirectionalSourceStatus.devices as device (device.deviceId)}
                    <li class="flex items-center justify-between gap-2 rounded-md bg-bgcolor p-2 text-sm">
                        <span>{device.deviceId} ({device.transferredBytes.toLocaleString()} bytes)</span>
                        <Button
                            size="sm"
                            styled="danger"
                            disabled={bidirectionalControlBusy || bidirectionalOperationRetained || device.revoked}
                            onclick={() => revokeBidirectionalDevice(device.deviceId)}
                        >{language.peerBidirectional.revoke}</Button>
                    </li>
                {/each}
            </ul>
        {/if}
    </div>

    <div class="mt-3 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerBidirectional.target}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerBidirectional.targetHelp}</p>
        <label class="mt-3 block text-sm font-bold" for="peer-bidirectional-target-uri">
            {language.peerBidirectional.pairingLink}
        </label>
        <textarea
            id="peer-bidirectional-target-uri"
            bind:value={bidirectionalPairingInput}
            rows="3"
            class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
        ></textarea>
        <Button
            className="mt-2"
            disabled={!bidirectionalEnabled
                || bidirectionalControlBusy
                || (bidirectionalOperationRetained
                    && ![
                        'awaitingConflict',
                        'targetPrepared',
                        'localCommitted',
                        'sourceUnavailable',
                    ].includes(bidirectionalOperationPhase))
                || ['prepared', 'running'].includes(bidirectionalSourceStatus.phase)
                || !bidirectionalPairingInput}
            onclick={syncBidirectional}
        >{language.peerBidirectional.sync}</Button>

        {#if bidirectionalOperationPhase === 'running'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerBidirectional.syncing}</p>
        {:else if bidirectionalOperationPhase === 'awaitingConflict' && bidirectionalResult?.kind === 'conflict'}
            <div class="mt-3 rounded-md border border-draculared p-3">
                <p class="font-bold text-draculared">{language.peerBidirectional.conflictTitle}</p>
                <p class="mt-1 text-sm text-textcolor2">{language.peerBidirectional.conflictReconnectHelp}</p>
                <ul class="mt-2 list-disc pl-5 text-sm">
                    {#each bidirectionalResult.conflicts as conflict (conflict.key)}
                        <li>{conflict.key}: {language.peerBidirectional[conflict.type]}</li>
                    {/each}
                </ul>
                <p class="mt-2 text-sm text-textcolor2">{language.peerBidirectional.backupWarning}</p>
                <div class="mt-2 flex flex-wrap gap-2">
                    <Button disabled={bidirectionalControlBusy} onclick={() => resolveBidirectional('local')}>
                        {language.peerBidirectional.keepLocal}
                    </Button>
                    <Button disabled={bidirectionalControlBusy} onclick={() => resolveBidirectional('remote')}>
                        {language.peerBidirectional.keepRemote}</Button>
                </div>
            </div>
        {:else if bidirectionalOperationPhase === 'sourceUnavailable'}
            <p class="mt-2 text-sm text-draculared">{language.peerBidirectional.sourceUnavailable}</p>
            <Button className="mt-2" disabled={bidirectionalControlBusy} onclick={resumeBidirectional}>
                {language.peerBidirectional.resume}
            </Button>
        {:else if bidirectionalOperationPhase === 'refreshPending'}
            <p class="mt-2 text-sm text-draculared">{language.peerBidirectional.refreshPending}</p>
            <Button className="mt-2" disabled={bidirectionalControlBusy} onclick={resumeBidirectional}>
                {language.peerBidirectional.retryRefresh}
            </Button>
        {:else if bidirectionalOperationPhase === 'localCommitted'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerBidirectional.resumeRequired}</p>
            <Button className="mt-2" disabled={bidirectionalControlBusy} onclick={resumeBidirectional}>
                {language.peerBidirectional.resume}
            </Button>
        {:else if bidirectionalOperationPhase === 'targetPrepared'}
            <Button className="mt-2" disabled={bidirectionalControlBusy} onclick={resumeBidirectional}>
                {language.peerBidirectional.resume}
            </Button>
        {:else if bidirectionalResult?.kind === 'noChanges'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerBidirectional.noChanges}</p>
        {:else if bidirectionalResult?.kind === 'updated'}
            <p class="mt-2 text-sm text-textcolor2">
                {language.peerBidirectional.updated(
                    bidirectionalResult.transferredObjects,
                    bidirectionalResult.transferredBytes.toLocaleString(),
                )}
            </p>
        {:else if bidirectionalResult?.kind === 'stale'}
            <p class="mt-2 text-sm text-draculared">{language.peerBidirectional.stale}</p>
        {/if}

        {#if [
                'localCommitted',
                'sourceUnavailable',
                'sourcePrepared',
                'targetPrepared',
                'awaitingConflict',
            ].includes(bidirectionalOperationPhase)
            && ['idle', 'stopped'].includes(bidirectionalSourceStatus.phase)}
            <Button className="mt-2" styled="danger" disabled={bidirectionalControlBusy} onclick={abandonBidirectional}>
                {language.peerBidirectional.abandon}
            </Button>
        {/if}

        {#if bidirectionalOperationPhase === 'completed'}
            <Button
                className="mt-2"
                disabled={bidirectionalControlBusy || ['prepared', 'running'].includes(bidirectionalSourceStatus.phase)}
                onclick={acknowledgeBidirectional}
            >
                {language.peerBidirectional.acknowledge}
            </Button>
        {/if}

        {#if bidirectionalBackups.length > 0}
            <h5 class="mt-3 font-bold">{language.peerBidirectional.backupCreated}</h5>
            <ul class="mt-1 list-disc pl-5 text-sm text-textcolor2">
                {#each bidirectionalBackups as backup (backup.packageId)}
                    <li>
                        {backup.side === 'local'
                            ? language.peerBidirectional.backupLocal
                            : language.peerBidirectional.backupRemote}: {backup.path}
                    </li>
                {/each}
            </ul>
        {/if}
    </div>

    {#if bidirectionalInputError}
        <p class="mt-3 text-sm text-draculared">{bidirectionalInputError}</p>
    {/if}
    {#if bidirectionalSourceError}
        <p class="mt-3 text-sm text-draculared">{bidirectionalSourceError}</p>
    {/if}
    {#if bidirectionalOperationError}
        <p class="mt-3 text-sm text-draculared">{bidirectionalOperationError}</p>
    {/if}
</section>
