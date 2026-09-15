<script lang="ts">
    import { onMount } from 'svelte'
    import Button from 'src/lib/UI/GUI/Button.svelte'
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
    import { exit } from '@tauri-apps/plugin-process'

    let panel = $state<HTMLDivElement | undefined>()
    let copied = $state('')
    const text = $derived(language.risuNest.update)
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
        if (model.open) panel?.focus()
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
        {:else if token.type === 'code'}<code class="rounded-sm bg-bgcolor px-1 py-0.5">{token.text}</code>
        {:else if token.type === 'link'}<button class="underline" type="button" onclick={() => openURL(token.url)}>{token.text}</button>
        {:else}{token.text}{/if}
    {/each}
{/snippet}

{#if model.open}
    <div role="presentation" class="fixed inset-0 z-[1001] flex items-center justify-center bg-black/60 p-4" onclick={dismissBackdrop} onkeydown={() => undefined}>
        <div bind:this={panel} tabindex="-1" role="dialog" aria-modal="true" aria-labelledby="app-update-title" class="flex max-h-[90dvh] w-full max-w-xl flex-col gap-4 overflow-y-auto rounded-lg border border-darkborderc bg-darkbg p-5 text-textcolor outline-hidden">
            <header>
                <h2 id="app-update-title" class="text-lg font-bold">{text.dialogTitle}</h2>
                {#if $appUpdateState.update}
                    <p class="mt-1 text-sm text-textcolor2">{$appUpdateState.environment?.currentVersion ?? ''} → {$appUpdateState.update.version} · {new Date($appUpdateState.update.pubDate).toLocaleDateString()}</p>
                {/if}
            </header>

            {#if $appUpdateState.phase === 'current'}
                <p>{text.current}</p>
            {:else if $appUpdateState.phase === 'disabled'}
                <p>{text.notConfigured}</p>
            {:else if $appUpdateState.phase === 'error'}
                <p class="text-danger-400" role="alert">{text.checkFailed}: {$appUpdateState.error}</p>
            {:else if $appUpdateState.update}
                {#if $appUpdateState.update.format === 'ipa'}<p class="rounded-md border border-darkborderc bg-bgcolor p-2 text-sm">{text.iosResign}</p>{/if}
                <div class="flex flex-col gap-2 text-sm leading-relaxed">
                    {#each noteBlocks as block}
                        {#if block.type === 'heading'}<h3 class="font-bold">{@render inline(block.content)}</h3>
                        {:else if block.type === 'paragraph'}<p>{@render inline(block.content)}</p>
                        {:else}<div class:pl-5={block.depth === 2} class="flex gap-2"><span>•</span><span>{@render inline(block.content)}</span></div>{/if}
                    {/each}
                </div>
                {#if model.busy}
                    <div class="flex flex-col gap-1" aria-live="polite">
                        <div role="progressbar" aria-label={text.downloading} aria-valuemin="0" aria-valuemax="100" aria-valuenow={model.percent ?? undefined} class="h-2 overflow-hidden rounded-md border border-darkborderc bg-bgcolor">
                            {#if model.percent === null}<div class="h-full w-full animate-pulse bg-borderc/60"></div>
                            {:else}<div class="h-full bg-borderc" style:width={`${model.percent}%`}></div>{/if}
                        </div>
                        <span class="text-xs text-textcolor2">{$appUpdateState.phase === 'applying' ? text.applying : text.downloading}</span>
                    </div>
                {/if}
                {#if $appUpdateState.stagedDeb}
                    <div class="flex flex-col gap-2 rounded-md border border-darkborderc bg-bgcolor p-3 text-sm">
                        <p>{text.debReady}</p>
                        <code class="break-all">{$appUpdateState.stagedDeb.path}</code>
                        <code class="break-all">{$appUpdateState.stagedDeb.installCommand}</code>
                        <div class="flex flex-wrap gap-2">
                            <Button styled="outlined" size="sm" onclick={() => void copy($appUpdateState.stagedDeb!.path)}>{copied === $appUpdateState.stagedDeb.path ? text.copied : text.copyPath}</Button>
                            <Button styled="outlined" size="sm" onclick={() => void copy($appUpdateState.stagedDeb!.installCommand)}>{copied === $appUpdateState.stagedDeb.installCommand ? text.copied : text.copyCommand}</Button>
                            <Button size="sm" onclick={() => void exit(0)}>{text.exitApp}</Button>
                        </div>
                    </div>
                {/if}
            {/if}

            <footer class="flex flex-wrap justify-end gap-2">
                {#if model.canCancel}
                    <Button styled="outlined" size="sm" onclick={() => void cancelAppUpdateDownload()}>{text.cancel}</Button>
                {:else if !model.busy && $appUpdateState.update && $appUpdateState.phase !== 'staged'}
                    <Button styled="outlined" size="sm" onclick={dismissAppUpdate}>{text.later}</Button>
                    <Button styled="outlined" size="sm" onclick={skipAppUpdate}>{text.skipVersion}</Button>
                    <Button size="sm" onclick={() => void applyAppUpdate()}>
                        {model.primaryAction === 'install' ? text.installNow : model.primaryAction === 'download' ? text.download : text.openDownload}
                    </Button>
                {:else if !model.busy}
                    <Button size="sm" onclick={dismissAppUpdate}>{text.close}</Button>
                {/if}
            </footer>
        </div>
    </div>
{/if}
