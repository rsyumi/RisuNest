<script lang="ts">
    import { CheckIcon, CopyIcon, LoaderCircleIcon, XIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import { buildNativeFileJobDialogModel } from 'src/ts/gui/nativeFileJobDialogModel'
    import {
        cancelActiveNativeFileOperation,
        dismissNativeFileOperationOutcome,
        nativeFileJobHost,
        nativeFileOperation,
        nativeFileOperationOutcome,
    } from 'src/ts/storage/nativeFileJobManager'

    let now = $state(Date.now())
    let panel = $state<HTMLDivElement | undefined>()
    let showDetails = $state(false)
    let copied = $state(false)

    const model = $derived(buildNativeFileJobDialogModel($nativeFileOperation, $nativeFileOperationOutcome, now))
    // Backup restores belong to the onboarding panel while it is up; content
    // imports stay here because the onboarding has no screen for them.
    const embedded = $derived($nativeFileJobHost === 'onboarding' && !model.compact)
    const open = $derived(model.open && !embedded)
    const ticking = $derived(open && model.terminal === null)
    const copy = $derived(language.risuNest.importDialog)

    $effect(() => {
        if (!ticking) return
        now = Date.now()
        const timer = setInterval(() => { now = Date.now() }, 1000)
        return () => clearInterval(timer)
    })

    $effect(() => {
        if (open) panel?.focus()
    })

    $effect(() => {
        if (model.terminal === null) {
            showDetails = false
            copied = false
        }
    })

    async function copyDetails(): Promise<void> {
        const details = model.terminal?.details
        if (!details) return
        try {
            await navigator.clipboard.writeText(details)
            copied = true
            setTimeout(() => { copied = false }, 1500)
        } catch {
            copied = false
        }
    }
</script>

{#if open}
    <div class="fixed inset-0 z-[1000] flex items-center justify-center bg-black/60 p-4">
        <div
            bind:this={panel}
            tabindex="-1"
            role="dialog"
            aria-modal="true"
            aria-labelledby="native-file-job-dialog-title"
            data-testid="native-file-job-dialog"
            class="flex max-h-[90dvh] w-full max-w-lg flex-col gap-4 overflow-y-auto rounded-lg border border-darkborderc bg-darkbg p-5 text-textcolor outline-hidden">
            <header class="flex flex-col gap-1">
                <div class="flex items-baseline justify-between gap-3">
                    <h2 id="native-file-job-dialog-title" class="text-lg font-bold">{model.title}</h2>
                    {#if model.subtitle}
                        <span class="shrink-0 text-sm text-textcolor2">{model.subtitle}</span>
                    {/if}
                </div>
                {#if model.sourceName || model.elapsed}
                    <div class="flex items-center justify-between gap-3 text-sm text-textcolor2">
                        <span class="truncate">{model.sourceName}{#if model.sourceName && model.sourceSize}{' · '}{model.sourceSize}{/if}</span>
                        <span class="shrink-0 tabular-nums">{model.elapsed}</span>
                    </div>
                {/if}
            </header>

            {#if model.terminal}
                <div class="flex flex-col gap-1 text-sm" role="status">
                    <p
                        class:text-borderc={model.terminal.state === 'succeeded'}
                        class:text-textcolor2={model.terminal.state === 'cancelled'}
                        class:text-danger-400={model.terminal.state === 'failed'}>
                        {model.terminal.summary}
                    </p>
                    {#if model.terminal.reason}
                        <p class="text-textcolor2">{model.terminal.reason}</p>
                    {/if}
                </div>
            {/if}

            <div class="flex flex-col gap-1" aria-live="polite">
                <div role="progressbar" aria-label={copy.titleImport} aria-valuemin={0} aria-valuemax={100} aria-valuenow={model.overallPercent ?? undefined} class="h-2 w-full overflow-hidden rounded-md border border-darkborderc bg-bgcolor">
                    {#if model.indeterminate}
                        <div class="h-full w-full bg-borderc/60 motion-safe:animate-pulse"></div>
                    {:else}
                        <div class="h-full bg-borderc transition-[width] duration-300" style:width={`${model.overallPercent ?? 0}%`}></div>
                    {/if}
                </div>
                <div class="flex justify-between text-xs tabular-nums text-textcolor2">
                    <span>{model.overallPercent !== null ? `${model.overallPercent}%` : model.terminal ? '' : copy.preparing}</span>
                    <span>{model.overallText}</span>
                </div>
            </div>

            {#if model.stages.length > 0}
                <ol class="flex flex-col gap-1.5 text-sm">
                    {#each (model.compact ? model.stages.filter(row => row.state === 'active' || row.state === 'stopped' || row.stage === 'complete') : model.stages) as row (row.stage)}
                        <li class="flex items-center gap-2" data-stage={row.stage} data-stage-state={row.state}>
                            {#if row.state === 'done'}
                                <CheckIcon size={16} class="shrink-0 text-borderc" />
                            {:else if row.state === 'active'}
                                <LoaderCircleIcon size={16} class="shrink-0 text-borderc motion-safe:animate-spin" />
                            {:else if row.state === 'stopped'}
                                <XIcon size={16} class="shrink-0 text-danger-400" />
                            {:else}
                                <span class="inline-block size-4 shrink-0 rounded-full border border-darkborderc"></span>
                            {/if}
                            <span class="grow truncate" class:text-textcolor2={row.state === 'pending'}>{row.label}</span>
                            {#if row.detail && !model.compact}
                                <span class="shrink-0 text-xs tabular-nums text-textcolor2">{row.detail}</span>
                            {/if}
                        </li>
                    {/each}
                </ol>
            {/if}

            {#if model.currentItem}
                <p class="truncate font-mono text-xs text-textcolor2">{model.currentItem}</p>
            {/if}

            {#if model.counters.length > 0}
                <dl class={model.compact ? "flex text-sm" : "grid grid-cols-2 gap-2 text-sm sm:grid-cols-4"}>
                    {#each model.counters as counter (counter.key)}
                        <div class="flex flex-col rounded-md border border-darkborderc bg-bgcolor px-2 py-1">
                            <dt class="text-xs text-textcolor2">{counter.label}</dt>
                            <dd class="tabular-nums">{counter.value}</dd>
                        </div>
                    {/each}
                </dl>
            {/if}

            {#if model.warnings.length > 0}
                <ul class="flex flex-col gap-1 rounded-md border border-danger-700 bg-bgcolor p-2 text-sm text-textcolor2">
                    {#each model.warnings as warning}
                        <li>{warning}</li>
                    {/each}
                </ul>
            {/if}

            {#if model.terminal?.details}
                <div class="flex flex-col items-start gap-2">
                    <Button styled="outlined" size="sm" onclick={() => { showDetails = !showDetails }}>
                        {copy.errorDetails}
                    </Button>
                    {#if showDetails}
                        <div class="relative w-full">
                            <button
                                type="button"
                                class="absolute top-1 right-1 rounded-sm p-1 text-textcolor2 hover:text-textcolor"
                                title={copied ? copy.copied : copy.copyDetails}
                                aria-label={copy.copyDetails}
                                onclick={copyDetails}>
                                {#if copied}
                                    <CheckIcon size={14} />
                                {:else}
                                    <CopyIcon size={14} />
                                {/if}
                            </button>
                            <pre class="max-h-48 overflow-auto rounded-md border border-darkborderc bg-bgcolor p-2 pr-8 text-xs whitespace-pre-wrap break-all text-textcolor2">{model.terminal.details}</pre>
                        </div>
                    {/if}
                </div>
            {/if}

            <footer class="flex flex-col items-end gap-1">
                {#if model.cancelVisible}
                    <Button styled="outlined" size="sm" disabled={!model.cancelEnabled} onclick={cancelActiveNativeFileOperation}>
                        {model.cancelLabel}
                    </Button>
                    {#if model.cancelNote}
                        <span class="text-xs text-textcolor2">{model.cancelNote}</span>
                    {/if}
                {/if}
                {#if model.closeVisible}
                    <Button size="sm" onclick={dismissNativeFileOperationOutcome}>{copy.close}</Button>
                {/if}
            </footer>
        </div>
    </div>
{/if}
