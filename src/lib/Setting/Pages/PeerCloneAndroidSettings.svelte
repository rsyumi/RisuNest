<script lang="ts">
    import { onMount } from 'svelte'

    import { language } from 'src/lang'
    import { alertConfirm } from 'src/ts/alert'
    import { loadPluginsAfterAuthoritativeRestore } from 'src/ts/plugins/plugins.svelte'
    import {
        acquireDestructiveReplacementFence,
        capturePersistentMutationToken,
    } from 'src/ts/storage/persistentDataRuntime.svelte'
    import {
        createAndroidPeerCloneFacade,
        type AndroidPeerCloneCapabilities,
        type AndroidPeerCloneState,
    } from 'src/ts/storage/sync/peerCloneAndroid'
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
    let capabilities = $state<AndroidPeerCloneCapabilities>()
    let cloneState = $state<AndroidPeerCloneState>(facade.getState())
    let pairingInput = $state('')
    let busy = $state(false)
    let error = $state('')
    let progressTimer: ReturnType<typeof setInterval> | undefined

    const targetEnabled = $derived(!!(
        capabilities?.productionEnabled
        && capabilities.androidClient
        && capabilities.atomicActivationReady
        && capabilities.losslessBackupReady
        && capabilities.httpTransportReady
    ))
    const progressMaximum = $derived(cloneState.totalBytes ?? Math.max(1, cloneState.completedBytes))

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
                    error = ''
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
        void Promise.all([facade.capabilities(), facade.recover()])
            .then(([available]) => {
                capabilities = available
                refreshState()
                initialized = true
                if (pendingUri) acceptPairingUri(pendingUri)
                if (cloneState.phase === 'downloading') {
                    beginProgressPolling()
                }
            })
            .catch(reportError)
        return () => {
            unsubscribe()
            if (progressTimer) clearInterval(progressTimer)
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
                disabled={busy || (cloneState.phase !== 'paused' && cloneState.phase !== 'downloading')}
                onclick={cancelClone}
            >{language.peerClone.cancel}</Button>
        </div>

        {#if cloneState.phase === 'paused' || cloneState.phase === 'downloading' || cloneState.phase === 'cancelled' || cloneState.phase === 'completed'}
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
