<script lang="ts">
    import { onMount, untrack } from 'svelte'
    import ErrorDetails from './ErrorDetails.svelte'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
    import SettingProgress from 'src/lib/Setting/RisuNest/SettingProgress.svelte'
    import { language } from 'src/lang'
    import { getDatabase } from 'src/ts/storage/database.svelte'
    import { openURL } from 'src/ts/globalApi.svelte'
    import { appUpdateState } from 'src/ts/update/state.svelte'
    import {
        applyAppUpdate,
        cancelAppUpdateDownload,
        dismissAppUpdate,
        skipAppUpdate,
    } from 'src/ts/update/controller'
    import { buildUpdateDialogModel } from 'src/ts/gui/updateDialogModel'
    import { parseReleaseNotes, selectLocalizedNotes, type ReleaseNoteInline } from 'src/ts/update/notes'
    import { getCurrentWindow } from '@tauri-apps/api/window'

    let panel = $state<HTMLDivElement | undefined>()
    let copied = $state('')
    const text = $derived(language.risuNest.update)
    const open = $derived($appUpdateState.popupVisible)
    const model = $derived(buildUpdateDialogModel($appUpdateState))
    const locale = $derived(getDatabase().language ?? 'en')
    const noteBlocks = $derived(parseReleaseNotes($appUpdateState.update
        ? selectLocalizedNotes($appUpdateState.update.localizedNotes, locale, $appUpdateState.update.notes)
        : ''))

    onMount(() => {
        const keydown = (event: KeyboardEvent) => {
            if (event.key === 'Escape' && model.open && !model.busy) dismissAppUpdate()
        }
        window.addEventListener('keydown', keydown)
        return () => window.removeEventListener('keydown', keydown)
    })

    $effect(() => {
        if (open && panel) untrack(() => panel?.focus())
    })

    async function copy(value: string): Promise<void> {
        await navigator.clipboard.writeText(value)
        copied = value
        setTimeout(() => { if (copied === value) copied = '' }, 1500)
    }

    function dismissBackdrop(event: MouseEvent): void {
        if (event.target === event.currentTarget && !model.busy) dismissAppUpdate()
    }
</script>

{#snippet inline(tokens: ReleaseNoteInline[])}
    {#each tokens as token}
        {#if token.type === 'strong'}<strong>{token.text}</strong>
        {:else if token.type === 'em'}<em>{token.text}</em>
        {:else if token.type === 'code'}<code class="rounded-sm bg-bgcolor px-1 py-0.5 whitespace-pre-wrap">{token.text}</code>
        {:else if token.type === 'link'}<button class="underline" type="button" onclick={() => openURL(token.url)}>{token.text}</button>
        {:else}{token.text}{/if}
    {/each}
{/snippet}

{#if model.open}
    <div role="presentation" class="fixed inset-0 z-work-dialog-update flex items-center justify-center bg-black/60 p-4" onclick={dismissBackdrop}>
        <div bind:this={panel} tabindex="-1" role="dialog" aria-modal="true" aria-labelledby="app-update-title" class="flex max-h-[90dvh] w-full max-w-lg flex-col gap-4 overflow-y-auto rounded-lg border border-darkborderc bg-darkbg p-5 text-textcolor outline-hidden">
            <header>
                <h2 id="app-update-title" class="text-lg font-bold">{text.dialogTitle}</h2>
                {#if $appUpdateState.update}
                    <p class="mt-1 text-sm text-subtext">{$appUpdateState.environment?.currentVersion ?? ''} → {$appUpdateState.update.version} · {new Date($appUpdateState.update.pubDate).toLocaleDateString()}</p>
                {/if}
            </header>

            {#if $appUpdateState.phase === 'checking'}
                <SettingProgress label={text.checking} />
            {:else if $appUpdateState.phase === 'current'}
                <p>{text.current}</p>
            {:else if $appUpdateState.phase === 'disabled'}
                <p>{text.notConfigured}</p>
            {:else if $appUpdateState.phase === 'error'}
                <p class="text-danger-400" role="alert">{text.checkFailed}: {$appUpdateState.error}</p>
                <ErrorDetails message={`${text.checkFailed}: ${$appUpdateState.error}`} />
            {:else if $appUpdateState.update}
                {#if $appUpdateState.update.format === 'ipa'}<p class="rounded-md border border-darkborderc bg-bgcolor p-2 text-sm">{text.iosResign}</p>{/if}
                <div class="flex flex-col gap-2 text-sm leading-relaxed">
                    {#each noteBlocks as block}
                        {#if block.type === 'heading'}<h3 class="font-bold" class:text-base={block.level <= 2}>{@render inline(block.content)}</h3>
                        {:else if block.type === 'paragraph'}<p>{@render inline(block.content)}</p>
                        {:else}<div class="flex gap-2" style:padding-left={`${(block.depth - 1) * 1.25}rem`}><span class="min-w-4 shrink-0 text-right">{block.marker}</span><span>{@render inline(block.content)}</span></div>{/if}
                    {/each}
                </div>
                {#if model.busy}
                    <SettingProgress
                        label={$appUpdateState.phase === 'applying' ? text.applying : text.downloading}
                        fraction={model.percent === null ? null : model.percent / 100}
                    />
                {/if}
                {#if $appUpdateState.stagedDeb}
                    <div class="flex flex-col gap-2 rounded-md border border-darkborderc bg-bgcolor p-3 text-sm">
                        <p>{text.debReady}</p>
                        <code class="break-all">{$appUpdateState.stagedDeb.path}</code>
                        <code class="break-all">{$appUpdateState.stagedDeb.installCommand}</code>
                        <div class="flex flex-wrap gap-2">
                            <SettingButton variant="secondary" onclick={() => void copy($appUpdateState.stagedDeb!.path)}>{copied === $appUpdateState.stagedDeb.path ? text.copied : text.copyPath}</SettingButton>
                            <SettingButton variant="secondary" onclick={() => void copy($appUpdateState.stagedDeb!.installCommand)}>{copied === $appUpdateState.stagedDeb.installCommand ? text.copied : text.copyCommand}</SettingButton>
                            <SettingButton onclick={() => void getCurrentWindow().close()}>{text.exitApp}</SettingButton>
                        </div>
                    </div>
                {/if}
            {/if}

            {#if model.canCancel || !model.busy}
                <footer class="flex flex-wrap justify-end gap-2">
                    {#if model.canCancel}
                        <SettingButton variant="secondary" onclick={() => void cancelAppUpdateDownload()}>{text.cancel}</SettingButton>
                    {:else if $appUpdateState.update && $appUpdateState.phase !== 'staged'}
                        <SettingButton variant="secondary" onclick={dismissAppUpdate}>{text.later}</SettingButton>
                        <SettingButton variant="secondary" onclick={skipAppUpdate}>{text.skipVersion}</SettingButton>
                        <SettingButton busy={model.busy} onclick={() => void applyAppUpdate()}>
                            {model.primaryAction === 'install' ? text.installNow : model.primaryAction === 'download' ? text.download : text.openDownload}
                        </SettingButton>
                    {:else}
                        <SettingButton variant={$appUpdateState.stagedDeb ? 'secondary' : 'primary'} onclick={dismissAppUpdate}>{text.close}</SettingButton>
                    {/if}
                </footer>
            {/if}
        </div>
    </div>
{/if}
