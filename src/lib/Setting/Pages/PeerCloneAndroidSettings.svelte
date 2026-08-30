<script lang="ts">
    import { onMount } from 'svelte'

    import { language } from 'src/lang'
    import { alertConfirm } from 'src/ts/alert'
    import { loadPluginsAfterAuthoritativeRestore } from 'src/ts/plugins/plugins.svelte'
    import {
        acquireDestructiveReplacementFence,
        capturePersistentMutationToken,
        flushPendingData,
    } from 'src/ts/storage/persistentDataRuntime.svelte'
    import {
        createAndroidPeerCloneFacade,
        type AndroidPeerCloneCapabilities,
        type AndroidPeerCloneState,
    } from 'src/ts/storage/sync/peerCloneAndroid'
    import { createAndroidPeerCloneSourceFacade, type AndroidPeerCloneSourceCapabilities } from 'src/ts/storage/sync/peerCloneAndroidSource'
    import { createAndroidPeerCloneSourceController } from 'src/ts/storage/sync/peerCloneAndroidSourceController'
    import type { PeerCloneSourceStatus } from 'src/ts/storage/sync/peerClone'
    import { createPeerDeltaFacade, parsePeerDeltaUri, type PeerDeltaCapabilities, type PeerDeltaPullResult, type PeerDeltaSourceStatus } from 'src/ts/storage/sync/peerDelta'
    import { createPeerDeltaController } from 'src/ts/storage/sync/peerDeltaController'
    import {
        consumePendingPeerCloneUri,
        subscribePeerCloneUri,
    } from 'src/ts/storage/sync/peerCloneDeepLink'
    import Button from 'src/lib/UI/GUI/Button.svelte'

    const facade = createAndroidPeerCloneFacade({
        runtime: {
            capturePersistentMutationToken,
            acquireDestructiveReplacementFence,
            afterRefresh: loadPluginsAfterAuthoritativeRestore,
        },
    })
    const sourceFacade = createAndroidPeerCloneSourceFacade({ flushPendingData })
    const sourceController = createAndroidPeerCloneSourceController(sourceFacade)
    const deltaController = createPeerDeltaController({
        facade: createPeerDeltaFacade({
            platform: 'android',
            runtime: {
                flushPendingData,
                capturePersistentMutationToken,
                acquirePersistentMutationFence: acquireDestructiveReplacementFence,
            },
        }),
    })
    let capabilities = $state<AndroidPeerCloneCapabilities>()
    let sourceCapabilities = $state<AndroidPeerCloneSourceCapabilities>()
    let sourceStatus = $state<PeerCloneSourceStatus>({ phase: 'idle', devices: [] })
    let deltaCapabilities = $state<PeerDeltaCapabilities>()
    let deltaSourceStatus = $state<PeerDeltaSourceStatus>({ phase: 'idle', devices: [] })
    let deltaPairingInput = $state('')
    let deltaPairingUri = $state('')
    let deltaResult = $state<PeerDeltaPullResult>()
    let cloneState = $state<AndroidPeerCloneState>(facade.getState())
    let pairingInput = $state('')
    let busy = $state(false)
    let error = $state('')
    let progressTimer: ReturnType<typeof setInterval> | undefined
    let sourceTimer: ReturnType<typeof setInterval> | undefined

    const targetEnabled = $derived(!!(
        capabilities?.productionEnabled
        && capabilities.androidClient
        && capabilities.atomicActivationReady
        && capabilities.losslessBackupReady
        && capabilities.httpTransportReady
    ))
    const progressMaximum = $derived(cloneState.totalBytes ?? Math.max(1, cloneState.completedBytes))
    const sourceEnabled = $derived(!!(
        sourceCapabilities?.productionEnabled
        && sourceCapabilities.sourceReady
        && sourceCapabilities.httpTransportReady
        && !sourceCapabilities.tunnelReady
    ))
    const deltaEnabled = $derived(!!(
        deltaCapabilities?.productionEnabled
        && deltaCapabilities.sourceReady
        && deltaCapabilities.atomicActivationReady
        && deltaCapabilities.authenticatedTransportReady
        && !deltaCapabilities.tunnelReady
    ))

    function refreshDelta(): void {
        const snapshot = deltaController.snapshot()
        deltaCapabilities = snapshot.capabilities
        deltaSourceStatus = snapshot.sourceStatus
        deltaPairingUri = snapshot.sourcePairingUri
        deltaResult = snapshot.pullResult
        if (snapshot.error) error = snapshot.error
    }

    async function prepareDelta(): Promise<void> {
        await withBusy(async () => { await deltaController.prepare(); refreshDelta() })
    }

    async function startDelta(): Promise<void> {
        if (!deltaSourceStatus.sessionId) return
        await withBusy(async () => { await deltaController.start(deltaSourceStatus.sessionId!); refreshDelta() })
    }

    async function stopDelta(): Promise<void> {
        if (!deltaSourceStatus.sessionId) return
        await withBusy(async () => { await deltaController.stop(deltaSourceStatus.sessionId!); refreshDelta() })
    }

    async function pullDelta(): Promise<void> {
        try {
            parsePeerDeltaUri(deltaPairingInput)
        } catch {
            error = language.peerDelta.invalidLink
            return
        }
        await withBusy(async () => { await deltaController.pull(deltaPairingInput); refreshDelta() })
    }

    async function prepareSource(): Promise<void> {
        await withBusy(async () => { sourceStatus = await sourceController.prepare() })
    }

    async function startSource(): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => { sourceStatus = await sourceController.start(sourceStatus.sessionId!) })
    }

    async function stopSource(): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => { sourceStatus = await sourceController.stop(sourceStatus.sessionId!) })
    }

    async function revokeSource(deviceId: string): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => { sourceStatus = await sourceController.revoke(sourceStatus.sessionId!, deviceId) })
    }

    function refreshState(): void {
        cloneState = facade.getState()
    }

    function reportError(cause: unknown): void {
        error = cause instanceof Error ? cause.message : String(cause)
    }

    function acceptPairingUri(uri: string): void {
        pairingInput = uri
        error = ''
        try {
            facade.join(uri)
            refreshState()
        } catch (cause) {
            error = cause instanceof Error && cause.message.includes('already owns')
                ? cause.message
                : language.peerClone.invalidLink
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

    function beginProgressPolling(): void {
        if (progressTimer) clearInterval(progressTimer)
        progressTimer = setInterval(() => {
            void facade.targetStatus()
                .then(() => {
                    refreshState()
                    error = cloneState.error ?? ''
                    if (
                        cloneState.phase === 'paused'
                        || cloneState.phase === 'completed'
                        || cloneState.phase === 'cancelled'
                        || cloneState.phase === 'failed'
                    ) {
                        if (progressTimer) clearInterval(progressTimer)
                        progressTimer = undefined
                    }
                })
                .catch((cause) => {
                    refreshState()
                    if (cloneState.phase === 'failed') {
                        if (progressTimer) clearInterval(progressTimer)
                        progressTimer = undefined
                    }
                    reportError(cause)
                })
        }, 500)
    }

    async function downloadClone(): Promise<void> {
        if (!await alertConfirm(language.peerClone.replacementConfirm)) return
        facade.confirmDestructiveReplace()
        await withBusy(async () => {
            await facade.download()
            beginProgressPolling()
        })
    }

    async function resumeClone(): Promise<void> {
        await withBusy(async () => {
            await facade.resume()
            beginProgressPolling()
        })
    }

    async function cancelClone(): Promise<void> {
        await withBusy(async () => {
            await facade.cancel()
            if (progressTimer) clearInterval(progressTimer)
            progressTimer = undefined
        })
    }

    onMount(() => {
        let initialized = false
        let pendingUri = consumePendingPeerCloneUri()
        const unsubscribe = subscribePeerCloneUri((uri) => {
            if (initialized) acceptPairingUri(uri)
            else pendingUri = uri
        })
        const unsubscribeDelta = deltaController.subscribe(() => refreshDelta())
        void Promise.all([facade.capabilities(), facade.recover(), sourceFacade.capabilities(), sourceController.refresh(), deltaController.initialize()])
            .then(([available, , availableSource, currentSource]) => {
                capabilities = available
                sourceCapabilities = availableSource
                sourceStatus = currentSource
                refreshState()
                initialized = true
                if (pendingUri) acceptPairingUri(pendingUri)
                if (cloneState.phase === 'downloading') {
                    beginProgressPolling()
                }
                sourceTimer = setInterval(() => {
                    void sourceController.refresh().then((current) => { sourceStatus = current })
                }, 1_000)
            })
            .catch(reportError)
        return () => {
            unsubscribe()
            unsubscribeDelta()
            if (progressTimer) clearInterval(progressTimer)
            if (sourceTimer) clearInterval(sourceTimer)
        }
    })
</script>

<section class="mt-4 rounded-md border border-darkborderc bg-darkbg p-3">
    <h3 class="text-xl font-bold">{language.peerClone.title}</h3>
    <p class="mt-1 text-sm text-textcolor2">{language.peerClone.description}</p>

    {#if capabilities && !targetEnabled}
        <p class="mt-3 rounded-md border border-borderc bg-bgcolor p-2 text-sm text-textcolor2">
            {language.peerClone.unavailable}
        </p>
    {/if}

    <div class="mt-3 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerClone.source}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerClone.sourceHelp}</p>
        <p class="mt-1 text-sm text-textcolor2">Trusted LAN only. Android tunnel modes are unavailable.</p>
        <div class="mt-2 flex flex-wrap gap-2">
            <Button disabled={!sourceEnabled || busy || sourceStatus.phase !== 'idle'} onclick={prepareSource}>
                {language.peerClone.prepare}
            </Button>
            <Button disabled={!sourceEnabled || busy || sourceStatus.phase !== 'prepared'} onclick={startSource}>
                {language.peerClone.start}
            </Button>
            <Button styled="danger" disabled={busy || !sourceStatus.sessionId} onclick={stopSource}>
                {language.peerClone.stop}
            </Button>
        </div>
        {#if sourceStatus.pairingUri}
            <textarea readonly rows="3" class="mt-2 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={sourceStatus.pairingUri}></textarea>
        {/if}
        {#each sourceStatus.devices as device (device.deviceId)}
            <div class="mt-2 flex items-center justify-between gap-2 text-sm">
                <span>{device.deviceId}</span>
                <Button styled="danger" disabled={busy || device.revoked} onclick={() => revokeSource(device.deviceId)}>
                    {language.peerClone.revoke}
                </Button>
            </div>
        {/each}
    </div>

    <div class="mt-3 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerClone.target}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerClone.targetHelp}</p>
        <label class="mt-2 block text-sm font-bold" for="android-peer-clone-target-uri">
            {language.peerClone.pairingLink}
        </label>
        <textarea
            id="android-peer-clone-target-uri"
            bind:value={pairingInput}
            rows="3"
            class="mt-1 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"
        ></textarea>
        <div class="mt-2 flex flex-wrap gap-2">
            <Button
                disabled={
                    busy
                    || !pairingInput
                    || cloneState.phase === 'paused'
                    || cloneState.phase === 'downloading'
                    || cloneState.phase === 'failed'
                }
                onclick={() => acceptPairingUri(pairingInput)}
            >
                {language.peerClone.useLink}
            </Button>
            <Button
                disabled={!targetEnabled || busy || cloneState.phase !== 'joined'}
                onclick={downloadClone}
            >{language.peerClone.download}</Button>
            <Button
                disabled={!targetEnabled || busy || cloneState.phase !== 'paused'}
                onclick={resumeClone}
            >{language.peerClone.resume}</Button>
            <Button
                styled="danger"
                disabled={
                    busy
                    || cloneState.activationCommitted
                    || (
                        cloneState.phase !== 'paused'
                        && cloneState.phase !== 'downloading'
                        && cloneState.phase !== 'failed'
                    )
                }
                onclick={cancelClone}
            >{language.peerClone.cancel}</Button>
        </div>

        {#if cloneState.phase === 'paused' || cloneState.phase === 'downloading' || cloneState.phase === 'failed' || cloneState.phase === 'cancelled' || cloneState.phase === 'completed'}
            <label class="mt-3 block text-sm font-bold" for="android-peer-clone-progress">
                {language.peerClone.progress}
            </label>
            <progress
                id="android-peer-clone-progress"
                class="mt-1 w-full"
                value={cloneState.completedBytes}
                max={progressMaximum}
            ></progress>
            <p class="text-sm text-textcolor2">
                {cloneState.completedBytes.toLocaleString()} / {cloneState.totalBytes?.toLocaleString() ?? '?'} bytes
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
    <p class="mt-1 text-sm text-textcolor2">Trusted LAN only. Android tunnel modes are unavailable.</p>

    <div class="mt-3 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerDelta.source}</h4>
        <div class="mt-2 flex flex-wrap gap-2">
            <Button disabled={!deltaEnabled || busy || !['idle', 'stopped'].includes(deltaSourceStatus.phase)} onclick={prepareDelta}>{language.peerDelta.prepare}</Button>
            <Button disabled={!deltaEnabled || busy || deltaSourceStatus.phase !== 'prepared'} onclick={startDelta}>{language.peerDelta.start}</Button>
            <Button styled="danger" disabled={busy || !deltaSourceStatus.sessionId} onclick={stopDelta}>{language.peerDelta.stop}</Button>
        </div>
        {#if deltaPairingUri}
            <textarea readonly rows="3" class="mt-2 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm" value={deltaPairingUri}></textarea>
        {/if}
    </div>

    <div class="mt-3 rounded-md border border-darkborderc p-3">
        <h4 class="font-bold">{language.peerDelta.target}</h4>
        <p class="mt-1 text-sm text-textcolor2">{language.peerDelta.divergenceHelp}</p>
        <textarea bind:value={deltaPairingInput} rows="3" class="mt-2 w-full rounded-md border border-darkborderc bg-bgcolor p-2 text-sm"></textarea>
        <Button className="mt-2" disabled={!deltaEnabled || busy || !deltaPairingInput} onclick={pullDelta}>{language.peerDelta.pull}</Button>
        {#if deltaResult?.kind === 'noChanges'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerDelta.noChanges}</p>
        {:else if deltaResult?.kind === 'updated'}
            <p class="mt-2 text-sm text-textcolor2">{language.peerDelta.updated(deltaResult.transferredObjects, deltaResult.transferredBytes.toLocaleString())}</p>
        {:else if deltaResult?.kind === 'fullCloneRequired'}
            <p class="mt-2 text-sm text-draculared">{language.peerDelta.fullCloneRequired}</p>
        {:else if deltaResult?.kind === 'conflict'}
            <p class="mt-2 text-sm text-draculared">{deltaResult.reason === 'localAndRemoteChanged' ? language.peerDelta.conflictLocalRemote : language.peerDelta.conflictStaleRevision}</p>
        {/if}
    </div>
</section>
