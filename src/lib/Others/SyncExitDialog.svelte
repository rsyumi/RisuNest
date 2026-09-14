<script lang="ts">
    import { AlertTriangleIcon, LoaderCircleIcon, SaveIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import {
        decideSyncExit,
        syncExitDialogState,
    } from 'src/ts/storage/syncExitProduction'

    let panel = $state<HTMLDivElement | undefined>()
    const dialogState = $derived($syncExitDialogState)
    const open = $derived(![
        'idle',
        'complete',
        'cancelled',
    ].includes(dialogState.phase))
    const needsChoice = $derived(
        dialogState.phase === 'local-failed'
        || dialogState.phase === 'edit-blocked'
        || dialogState.phase === 'remote-delayed'
        || dialogState.phase === 'remote-blocked',
    )
    const localFailure = $derived(dialogState.phase === 'local-failed')
    const editBlocked = $derived(dialogState.phase === 'edit-blocked')
    const copy = $derived(language.risuNest.exitDrain)

    const title = $derived(
        localFailure ? copy.saveFailedTitle
            : editBlocked ? copy.editBlockedTitle
                : copy.title,
    )
    const detail = $derived.by(() => {
        switch (dialogState.phase) {
            case 'saving': return copy.saving
            case 'edit-blocked': return copy.editBlocked
            case 'capturing': return copy.capturing
            case 'syncing': return copy.syncing
            case 'remote-delayed': return copy.delayed
            case 'remote-blocked': return copy.blocked
            case 'local-failed': return copy.saveFailed
            default: return ''
        }
    })

    $effect(() => {
        if (open) panel?.focus()
    })
</script>

<svelte:window onkeydown={(event) => {
    if (open && needsChoice && event.key === 'Escape') {
        event.preventDefault()
        decideSyncExit('cancel-exit')
    }
}} />

{#if open}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/65 p-4 backdrop-blur-[2px]">
        <div
            bind:this={panel}
            tabindex="-1"
            role="dialog"
            aria-modal="true"
            aria-labelledby="sync-exit-title"
            aria-describedby="sync-exit-detail"
            data-testid="sync-exit-dialog"
            class="w-full max-w-md overflow-hidden rounded-xl border border-darkborderc bg-darkbg text-textcolor shadow-2xl outline-hidden">
            <div class="h-1 bg-borderc" class:motion-safe:animate-pulse={!needsChoice}></div>
            <div class="flex flex-col gap-5 p-5 sm:p-6">
                <header class="flex items-start gap-3">
                    <div class="mt-0.5 flex size-10 shrink-0 items-center justify-center rounded-full border border-darkborderc bg-bgcolor">
                        {#if localFailure || editBlocked || dialogState.phase === 'remote-blocked'}
                            <AlertTriangleIcon size={20} class="text-danger-400" aria-hidden="true" />
                        {:else if dialogState.phase === 'saving' || dialogState.phase === 'capturing'}
                            <SaveIcon size={20} class="text-borderc" aria-hidden="true" />
                        {:else}
                            <LoaderCircleIcon size={20} class="text-borderc motion-safe:animate-spin" aria-hidden="true" />
                        {/if}
                    </div>
                    <div class="min-w-0 grow">
                        <h2 id="sync-exit-title" class="text-lg font-semibold leading-tight">{title}</h2>
                        <p id="sync-exit-detail" class="mt-1.5 text-sm leading-relaxed text-textcolor2" aria-live="polite">
                            {detail}
                        </p>
                    </div>
                </header>

                {#if !needsChoice}
                    <div class="h-1.5 overflow-hidden rounded-full border border-darkborderc bg-bgcolor" role="progressbar" aria-label={detail}>
                        <div class="sync-exit-progress h-full w-2/5 rounded-full bg-borderc"></div>
                    </div>
                {:else}
                    <div class="flex flex-col-reverse gap-2 sm:flex-row sm:flex-wrap sm:justify-end">
                        <Button styled="outlined" onclick={() => decideSyncExit('cancel-exit')}>
                            {copy.cancelExit}
                        </Button>
                        {#if !editBlocked}
                            <Button styled="danger" onclick={() => decideSyncExit('exit-unsynced')}>
                                {localFailure ? copy.exitWithoutSaving : copy.exitWithoutSync}
                            </Button>
                        {/if}
                        <Button onclick={() => decideSyncExit('wait')}>
                            {localFailure ? copy.retrySaving : copy.keepWaiting}
                        </Button>
                    </div>
                {/if}
            </div>
        </div>
    </div>
{/if}

<style>
    @keyframes sync-exit-progress {
        0% { transform: translateX(-105%); }
        50% { transform: translateX(80%); }
        100% { transform: translateX(255%); }
    }

    .sync-exit-progress {
        animation: sync-exit-progress 1.4s ease-in-out infinite;
    }

    @media (prefers-reduced-motion: reduce) {
        .sync-exit-progress { animation: none; }
    }
</style>
