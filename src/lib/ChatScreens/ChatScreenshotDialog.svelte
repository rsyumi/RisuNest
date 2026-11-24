<script lang="ts">
    import { untrack } from 'svelte'
    import { XIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import {
        fullScreenshotRange,
        recentScreenshotRange,
        validateScreenshotRange,
    } from 'src/ts/chatScreenshotRange'
    import NumberInput from '../UI/GUI/NumberInput.svelte'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import ChatWindowLayer from './ChatWindowLayer.svelte'

    interface Props {
        totalTurns: number
        running?: boolean
        completedTurns?: number
        error?: string
        onStart: (start: number, end: number) => void
        onCancel: () => void
        onClose: () => void
    }

    let {
        totalTurns,
        running = false,
        completedTurns = 0,
        error = '',
        onStart,
        onCancel,
        onClose,
    }: Props = $props()

    const initialRange = untrack(() => recentScreenshotRange(totalTurns))
    let start = $state(initialRange.start)
    let end = $state(initialRange.end)
    let cancellationRequested = false
    let validation = $derived(validateScreenshotRange(totalTurns, start, end))
    let selectedTurns = $derived(validation.ok ? validation.end - validation.start + 1 : 0)
    // The live range keeps reacting to the inputs and to a parent that retotals
    // the conversation, so the progress line uses the count captured when the
    // run started instead of the current selection.
    let runTurns = $state<number | null>(null)
    let progressTurns = $derived(runTurns ?? selectedTurns)
    let progressTotal = $derived(Math.max(1, progressTurns))
    let progressPercent = $derived(
        Math.min(100, Math.max(0, (completedTurns / progressTotal) * 100)),
    )

    const button =
        'rounded-md border border-darkborderc px-4 py-2 transition-colors hover:bg-selected'

    function applyRange(range: { start: number; end: number }) {
        start = range.start
        end = range.end
    }

    function validationMessage() {
        if (validation.ok === true) return ''
        switch (validation.reason) {
            case 'empty': return language.screenshotEmpty
            case 'integer': return language.screenshotIntegerRange
            case 'bounds': return language.screenshotRangeBounds
            case 'order': return language.screenshotRangeOrder
        }
    }

    function closeDialog() {
        if (running) cancelCapture()
        onClose()
    }

    function cancelCapture() {
        if (cancellationRequested) return
        cancellationRequested = true
        onCancel()
    }

    $effect(() => {
        if (!running) {
            cancellationRequested = false
            runTurns = null
            return
        }
        runTurns = untrack(() => selectedTurns)
        return cancelCapture
    })

</script>

<ChatWindowLayer>
<!-- Work dialog layer: alerts and pickers stack above it on the shared modal layer. -->
<div class="fixed inset-0 z-[1000] flex items-center justify-center bg-black/50 p-4" role="presentation">
    <div
        class="w-full max-w-md max-h-full overflow-y-auto rounded-md bg-darkbg p-4 text-textcolor"
        role="dialog"
        aria-modal="true"
        aria-labelledby="chat-screenshot-title"
        use:modalNavigation={{ close: () => running ? cancelCapture() : onClose() }}
    >
        <div class="flex items-center justify-between gap-4">
            <h2 id="chat-screenshot-title" class="text-lg font-semibold">{language.screenshot}</h2>
            <button type="button" class="shrink-0 cursor-pointer text-textcolor2 transition-colors hover:text-green-500" onclick={closeDialog} aria-label={language.cancel}><XIcon size={24} /></button>
        </div>

        <p class="mt-2 text-sm text-textcolor2">
            {language.screenshotTurns.replace('{total}', String(totalTurns))}
        </p>
        <p class="mt-1 text-xs text-textcolor2">{language.screenshotConversationStartNote}</p>

        {#if running}
            <div class="mt-5" aria-live="polite">
                <div class="mb-2 text-sm">
                    {language.screenshotProgress
                        .replace('{completed}', String(completedTurns))
                        .replace('{total}', String(progressTurns))}
                </div>
                <div
                    class="h-1.5 w-full overflow-hidden rounded-full bg-darkbutton"
                    role="progressbar"
                    aria-valuemin={0}
                    aria-valuemax={progressTotal}
                    aria-valuenow={completedTurns}
                >
                    <div class="h-full rounded-full bg-borderc" style="width: {progressPercent}%"></div>
                </div>
                <button
                    type="button"
                    data-cancel
                    class="{button} mt-4 w-full bg-darkbutton"
                    onclick={cancelCapture}
                >{language.cancel}</button>
            </div>
        {:else}
            <fieldset class="mt-5" disabled={totalTurns === 0}>
                <legend class="mb-2 text-sm font-medium">{language.screenshotInclusiveRange}</legend>
                <div class="grid grid-cols-2 gap-3">
                    <label class="flex flex-col gap-1 text-sm">
                        {language.screenshotStart}
                        <NumberInput size="md" fullwidth min={1} max={totalTurns} bind:value={start} />
                    </label>
                    <label class="flex flex-col gap-1 text-sm">
                        {language.screenshotEnd}
                        <NumberInput size="md" fullwidth min={1} max={totalTurns} bind:value={end} />
                    </label>
                </div>
                <div class="mt-3 grid grid-cols-2 gap-3">
                    <button type="button" data-recent class="{button} bg-darkbutton" onclick={() => applyRange(recentScreenshotRange(totalTurns))}>{language.screenshotRecent50}</button>
                    <button type="button" data-full class="{button} bg-darkbutton" onclick={() => applyRange(fullScreenshotRange(totalTurns))}>{language.screenshotFull}</button>
                </div>
            </fieldset>

            {#if !validation.ok}
                <p class="mt-3 text-sm text-draculared" role="alert">{validationMessage()}</p>
            {/if}
            {#if error}
                <p class="mt-3 text-sm text-draculared" role="alert">{error}</p>
            {/if}

            <button
                type="button"
                data-capture
                class="{button} mt-5 w-full bg-selected disabled:cursor-not-allowed disabled:opacity-50"
                disabled={!validation.ok}
                onclick={() => validation.ok && onStart(validation.start, validation.end)}
            >{language.screenshotCapture}</button>
        {/if}
    </div>
</div>
</ChatWindowLayer>
