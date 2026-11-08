<script lang="ts">
    import { CheckIcon, CopyIcon, LoaderCircleIcon, XIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { diagnosticInfo, formatErrorDetails } from 'src/ts/diagnostics'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
    import SettingProgress from 'src/lib/Setting/RisuNest/SettingProgress.svelte'
    import { buildNativeFileJobDialogModel, failureReason } from 'src/ts/gui/nativeFileJobDialogModel'
    import {
        cancelActiveNativeFileOperation,
        dismissNativeFileOperationOutcome,
        retryPortableRestoreBodiesFromOutcome,
        nativeFileJobHost,
        nativeFileOperation,
        nativeFileOperationOutcome,
    } from 'src/ts/storage/nativeFileJobManager'

    import { retrySnapshotRestoreBodiesFromOutcome } from 'src/ts/storage/nativePersistentMaintenance'

    const accountRetryAvailable = $derived(!!$nativeFileOperationOutcome?.status?.officialAssets?.operationId
        && ['failed', 'cancelled'].includes($nativeFileOperationOutcome.status.officialAssets.state)
        && !['account-changed', 'library-changed'].includes($nativeFileOperationOutcome.status.officialAssets.reason ?? ''))

    const snapshotRetryAvailable = $derived($nativeFileOperationOutcome?.status?.kind === 'snapshot-bodies'
        && ['failed', 'cancelled'].includes($nativeFileOperationOutcome.status.state))

    let now = $state(Date.now())
    let panel = $state<HTMLDivElement | undefined>()
    let showDetails = $state(false)
    let copied = $state(false)

    const model = $derived(buildNativeFileJobDialogModel($nativeFileOperation, $nativeFileOperationOutcome, now))
    const detailsText = $derived(formatErrorDetails($diagnosticInfo, model.terminal?.details || model.terminal?.reason || model.terminal?.summary || ''))
    // Backup restores belong to the onboarding panel while it is up; content
    // imports stay here because the onboarding has no screen for them.
    const embedded = $derived($nativeFileJobHost === 'onboarding' && !model.compact)
    const open = $derived(model.open && !embedded)
    const ticking = $derived(open && model.terminal === null)
    const copy = $derived(language.risuNest.importDialog)
    const retryAvailable = $derived($nativeFileOperationOutcome?.status?.kind === 'restore-portable-backup'
        && $nativeFileOperationOutcome.status.restoreAdoptionConfirmed === true
        && $nativeFileOperationOutcome.status.portableBodyRetry?.pending === true
        && $nativeFileOperationOutcome.status.portableBodyRetry.available === true
        && !$nativeFileOperationOutcome.status.portableBodyRetry.sourceRequired)

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
        const details = model.terminal ? detailsText : ''
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
                        <span class="shrink-0 text-sm text-subtext">{model.subtitle}</span>
                    {/if}
                </div>
                {#if model.sourceName || model.elapsed}
                    <div class="flex items-center justify-between gap-3 text-sm text-subtext">
                        <span class="truncate">{model.sourceName}{#if model.sourceName && model.sourceSize}{' · '}{model.sourceSize}{/if}</span>
                        <span class="shrink-0 tabular-nums">{model.elapsed}</span>
                    </div>
                {/if}
            </header>

            {#if model.terminal}
                <div class="flex flex-col gap-1 text-sm" role="status">
                    <p
                        class:text-success-500={model.terminal.state === 'succeeded'}
                        class:text-subtext={model.terminal.state === 'cancelled'}
                        class:text-danger-400={model.terminal.state === 'failed'}>
                        {model.terminal.summary}
                    </p>
                    {#if model.terminal.reason}
                        <p class="text-subtext">{model.terminal.reason}</p>
                    {/if}
                    {#if model.terminal.location}
                        <p class="text-subtext break-words">{model.terminal.location}</p>
                    {/if}
                    {#if model.terminal.failedFiles.length > 0}
                        <ul class="mt-1 flex max-h-40 flex-col gap-1 overflow-y-auto" data-testid="native-file-job-failed-files">
                            {#each model.terminal.failedFiles as file, index (index)}
                                <li class="flex flex-col">
                                    <span class="truncate">{file.name}</span>
                                    <span class="text-subtext">{file.reason}</span>
                                </li>
                            {/each}
                        </ul>
                    {/if}
                </div>
            {/if}

            <SettingProgress
                label={model.overallPercent === null && !model.terminal ? copy.preparing : model.title}
                detail={model.overallText}
                fraction={model.indeterminate ? null : (model.overallPercent ?? 0) / 100}
                done={model.terminal?.state === 'succeeded'}
                stopped={model.terminal !== null && model.terminal.state !== 'succeeded'}
            />

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
                            <span class="grow truncate" class:text-subtext={row.state === 'pending'}>{row.label}</span>
                            {#if row.detail && !model.compact}
                                <span class="shrink-0 text-xs tabular-nums text-subtext">{row.detail}</span>
                            {/if}
                        </li>
                    {/each}
                </ol>
            {/if}

            {#if model.currentItem}
                <p class="truncate font-mono text-xs text-subtext">{model.currentItem}</p>
            {/if}

            {#if model.counters.length > 0}
                <dl class={model.compact ? "flex text-sm" : "grid grid-cols-2 gap-2 text-sm sm:grid-cols-4"}>
                    {#each model.counters as counter (counter.key)}
                        <div class="flex flex-col rounded-md border border-darkborderc bg-bgcolor px-2 py-1">
                            <dt class="text-xs text-subtext">{counter.label}</dt>
                            <dd class="tabular-nums">{counter.value}</dd>
                        </div>
                    {/each}
                </dl>
            {/if}

            {#if model.warnings.length > 0}
                <ul class="flex flex-col gap-1 rounded-md border border-danger-400/50 bg-bgcolor p-2 text-sm">
                    {#each model.warnings as warning}
                        <li>{warning}</li>
                    {/each}
                </ul>
            {/if}

            {#if model.terminal && (model.terminal.details || model.terminal.state === 'failed')}
                <div class="flex flex-col items-start gap-2">
                    <SettingButton variant="secondary" onclick={() => { showDetails = !showDetails }}>
                        {showDetails ? copy.errorDetailsHide : copy.errorDetails}
                    </SettingButton>
                    {#if showDetails}
                        <div class="relative w-full">
                            <button
                                type="button"
                                class="absolute top-1 right-1 rounded-sm p-1 text-subtext hover:text-textcolor"
                                title={copied ? copy.copied : copy.copyDetails}
                                aria-label={copy.copyDetails}
                                onclick={copyDetails}>
                                {#if copied}
                                    <CheckIcon size={14} />
                                {:else}
                                    <CopyIcon size={14} />
                                {/if}
                            </button>
                            <pre class="max-h-48 overflow-auto rounded-md border border-darkborderc bg-bgcolor p-2 pr-8 text-xs whitespace-pre-wrap break-all text-subtext">{detailsText}</pre>
                        </div>
                    {/if}
                </div>
            {/if}

            {#if model.cancelVisible || model.closeVisible}
                <footer class="flex flex-wrap items-center justify-end gap-2">
                    {#if model.cancelVisible}
                        {#if model.cancelNote}
                            <span class="mr-auto text-xs text-subtext">{model.cancelNote}</span>
                        {/if}
                        <SettingButton variant="secondary" disabled={!model.cancelEnabled} onclick={cancelActiveNativeFileOperation}>
                            {model.cancelLabel}
                        </SettingButton>
                    {/if}
                    {#if model.closeVisible}
                        {#if accountRetryAvailable}
                            <SettingButton variant="secondary" onclick={() => {
                                const startedAt = Date.now()
                                void import('src/ts/storage/sync/nativeOfficialAccountOperations')
                                    .then(({ retryNativeOfficialAccountAssetsFromOutcome }) => retryNativeOfficialAccountAssetsFromOutcome()).catch(async error => {
                                        const { fileOperationErrorWasPresented } = await import('src/ts/storage/fileOperationErrorPresentation')
                                        if (fileOperationErrorWasPresented('import', startedAt)) return
                                        const { alertError } = await import('src/ts/alert')
                                        alertError(failureReason(error?.code ?? ''))
                                    })
                            }}>{language.retry}</SettingButton>
                        {/if}
                        {#if snapshotRetryAvailable}
                            <SettingButton variant="secondary" onclick={() => {void retrySnapshotRestoreBodiesFromOutcome().catch(async error => {
                                const { alertError } = await import('src/ts/alert')
                                alertError(failureReason(error?.code ?? ''))
                            })}}>{language.retry}</SettingButton>
                        {/if}
                        {#if retryAvailable}
                            <SettingButton variant="secondary" onclick={() => {void retryPortableRestoreBodiesFromOutcome().catch(() => {})}}>{language.retry}</SettingButton>
                        {/if}
                        <SettingButton onclick={() => {void Promise.resolve(dismissNativeFileOperationOutcome()).catch(async error => {
                            const { alertError } = await import('src/ts/alert')
                            alertError(failureReason(error?.code ?? ''))
                        })}}>{copy.close}</SettingButton>
                    {/if}
                </footer>
            {/if}
        </div>
    </div>
{/if}
