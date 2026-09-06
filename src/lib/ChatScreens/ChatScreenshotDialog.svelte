<script lang="ts">
    import { untrack } from 'svelte'
    import { language } from 'src/lang'
    import {
        fullScreenshotRange,
        recentScreenshotRange,
        validateScreenshotRange,
    } from 'src/ts/chatScreenshotRange'

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

    function handleKeydown(event: KeyboardEvent) {
        // The global hotkey handler already calls preventDefault on Escape from
        // its own document listener, so defaultPrevented cannot gate this.
        if (event.key !== 'Escape') return
        event.preventDefault()
        if (running) {
            cancelCapture()
            return
        }
        onClose()
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

<svelte:window onkeydown={handleKeydown} />

<div class="fixed inset-0 z-[1000] flex items-center justify-center bg-black/50 p-4" role="presentation">
    <div
        class="w-full max-w-md max-h-full overflow-y-auto rounded-lg border border-darkborderc bg-darkbg p-5 text-textcolor shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-labelledby="chat-screenshot-title"
    >
        <div class="flex items-center justify-between gap-4">
            <h2 id="chat-screenshot-title" class="text-lg font-semibold">{language.screenshot}</h2>
            <button type="button" class="-mr-2 flex h-10 w-10 shrink-0 items-center justify-center rounded-md text-xl text-textcolor2 hover:text-textcolor" onclick={closeDialog} aria-label={language.cancel}>×</button>
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
                <progress class="w-full" max={Math.max(1, progressTurns)} value={completedTurns}></progress>
                <button
                    type="button"
                    data-cancel
                    class="mt-4 w-full rounded-md border border-darkborderc bg-darkbutton px-4 py-2 hover:bg-selected"
                    onclick={cancelCapture}
                >{language.cancel}</button>
            </div>
        {:else}
            <fieldset class="mt-5" disabled={totalTurns === 0}>
                <legend class="mb-2 text-sm font-medium">{language.screenshotInclusiveRange}</legend>
                <div class="grid grid-cols-2 gap-3">
                    <label class="flex flex-col gap-1 text-sm">
                        {language.screenshotStart}
                        <input class="rounded-md border border-darkborderc bg-bgcolor px-3 py-2" type="number" min="1" max={totalTurns} step="1" bind:value={start} />
                    </label>
                    <label class="flex flex-col gap-1 text-sm">
                        {language.screenshotEnd}
                        <input class="rounded-md border border-darkborderc bg-bgcolor px-3 py-2" type="number" min="1" max={totalTurns} step="1" bind:value={end} />
                    </label>
                </div>
                <div class="mt-3 grid grid-cols-2 gap-3">
                    <button type="button" data-recent class="rounded-md border border-darkborderc px-3 py-2 hover:bg-selected" onclick={() => applyRange(recentScreenshotRange(totalTurns))}>{language.screenshotRecent50}</button>
                    <button type="button" data-full class="rounded-md border border-darkborderc px-3 py-2 hover:bg-selected" onclick={() => applyRange(fullScreenshotRange(totalTurns))}>{language.screenshotFull}</button>
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
                class="mt-5 w-full rounded-md bg-selected px-4 py-2 disabled:cursor-not-allowed disabled:opacity-50"
                disabled={!validation.ok}
                onclick={() => validation.ok && onStart(validation.start, validation.end)}
            >{language.screenshotCapture}</button>
        {/if}
    </div>
</div>
