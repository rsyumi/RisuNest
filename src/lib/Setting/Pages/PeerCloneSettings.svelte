<script lang="ts">
    import { onMount } from 'svelte'

    import { language } from 'src/lang'
    import { alertConfirm } from 'src/ts/alert'
    import {
        createPeerCloneFacade,
        initialPeerCloneState,
        pairingUriForQr,
        type PeerCloneNativeCapabilities,
        type PeerCloneSourceStatus,
        type PeerCloneState,
    } from 'src/ts/storage/sync/peerClone'
    import {
        consumePendingPeerCloneUri,
        subscribePeerCloneUri,
    } from 'src/ts/storage/sync/peerCloneDeepLink'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import {
        acquireDestructiveReplacementFence,
        capturePersistentMutationToken,
        flushPendingData,
    } from 'src/ts/storage/persistentDataRuntime.svelte'

    const facade = createPeerCloneFacade({
        platform: 'desktop',
        runtime: {
            flushPendingData,
            capturePersistentMutationToken,
            acquireDestructiveReplacementFence,
        },
    })
    let capabilities = $state<PeerCloneNativeCapabilities>()
    let sourceStatus = $state<PeerCloneSourceStatus>({ phase: 'idle', devices: [] })
    let cloneState = $state<PeerCloneState>(initialPeerCloneState)
    let pairingInput = $state('')
    let sourcePairingUri = $state('')
    let qrDataUrl = $state('')
    let busy = $state(false)
    let error = $state('')
    let progressTimer: ReturnType<typeof setInterval> | undefined
    let sourceTimer: ReturnType<typeof setInterval> | undefined

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
        cloneState = facade.getState()
    }

    function acceptPairingUri(uri: string): void {
        pairingInput = uri
        error = ''
        try {
            facade.join(uri)
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
            sourceStatus = await facade.prepare()
        })
    }

    async function startSource(): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => {
            sourceStatus = await facade.start(sourceStatus.sessionId!)
            sourcePairingUri = sourceStatus.pairingUri ?? ''
            if (sourcePairingUri) {
                const { default: QRCode } = await import('qrcode')
                qrDataUrl = await QRCode.toDataURL(pairingUriForQr(sourcePairingUri), { margin: 1, width: 256 })
            }
            beginSourcePolling()
        })
    }

    async function stopSource(): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => {
            await facade.stop(sourceStatus.sessionId!)
            sourceStatus = { ...sourceStatus, phase: 'stopped', devices: [] }
            sourcePairingUri = ''
            qrDataUrl = ''
            if (sourceTimer) clearInterval(sourceTimer)
            sourceTimer = undefined
        })
    }

    async function revokeDevice(deviceId: string): Promise<void> {
        if (!sourceStatus.sessionId) return
        await withBusy(async () => {
            await facade.revoke(sourceStatus.sessionId!, deviceId)
            sourceStatus = await facade.sourceStatus()
        })
    }

    function beginProgressPolling(): void {
        if (progressTimer) clearInterval(progressTimer)
        progressTimer = setInterval(() => {
            void facade.targetStatus()
                .then(() => {
                    refreshState()
                    if (cloneState.target.phase === 'completed' || cloneState.target.phase === 'failed') {
                        if (progressTimer) clearInterval(progressTimer)
                        progressTimer = undefined
                    }
                })
                .catch((cause) => {
                    refreshState()
                    if (cloneState.target.phase === 'failed') {
                        if (progressTimer) clearInterval(progressTimer)
                        progressTimer = undefined
                    }
                    reportError(cause)
                })
        }, 500)
    }

    function beginSourcePolling(): void {
        if (sourceTimer) clearInterval(sourceTimer)
        sourceTimer = setInterval(() => {
            void facade.sourceStatus()
                .then((value) => {
                    sourceStatus = value
                    if (value.phase !== 'running') {
                        if (sourceTimer) clearInterval(sourceTimer)
                        sourceTimer = undefined
                    }
                })
                .catch((cause) => {
                    if (sourceTimer) clearInterval(sourceTimer)
                    sourceTimer = undefined
                    reportError(cause)
                })
        }, 1000)
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
        const pendingUri = consumePendingPeerCloneUri()
        if (pendingUri) acceptPairingUri(pendingUri)
        const unsubscribe = subscribePeerCloneUri(acceptPairingUri)
        void facade.capabilities().then((value) => {
            capabilities = value
        }).catch(reportError)
        return () => {
            unsubscribe()
            if (progressTimer) clearInterval(progressTimer)
            if (sourceTimer) clearInterval(sourceTimer)
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
        <div class="mt-2 flex flex-wrap gap-2">
            <Button disabled={!sourceEnabled || busy} onclick={prepareSource}>{language.peerClone.prepare}</Button>
            <Button
                disabled={!sourceEnabled || busy || sourceStatus.phase !== 'prepared'}
                onclick={startSource}
            >{language.peerClone.start}</Button>
            <Button
                styled="danger"
                disabled={busy || sourceStatus.phase !== 'running'}
                onclick={stopSource}
            >{language.peerClone.stop}</Button>
        </div>

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
