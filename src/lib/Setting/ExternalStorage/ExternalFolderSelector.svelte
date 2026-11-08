<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { ChevronRightIcon, FolderIcon, XIcon } from '@lucide/svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import { getExternalStorageBridge } from 'src/ts/storage/sync/external/bridge'
    import type { ExternalFolderEntry, ExternalFolderSelection } from 'src/ts/storage/sync/external/types'
    import { externalErrorMessage, type ExternalStorageStrings } from './strings'

    interface Props {
        strings: ExternalStorageStrings
        selectionId: string
        onselected: (selection: ExternalFolderSelection) => void
        oncancel: () => void
    }

    let { strings, selectionId, onselected, oncancel }: Props = $props()
    const bridge = getExternalStorageBridge()

    let path = $state<ExternalFolderEntry[]>([])
    let folders = $state<ExternalFolderEntry[]>([])
    let nextCursor = $state<string | undefined>()
    let selectable = $state(false)
    let loading = $state(false)
    let loadError = $state('')
    let selecting = $state(false)
    let selectError = $state('')
    let title = $state<HTMLHeadingElement | undefined>()
    let crumbs = $state<HTMLElement | undefined>()
    let list = $state<HTMLUListElement | undefined>()
    let sentinel = $state<HTMLLIElement | undefined>()
    let navigation = 0
    let lastRequest: { folder?: string; cursor?: string } = {}
    let finished = false

    const current = $derived(path.at(-1))
    const canSelect = $derived(selectable && current !== undefined && !loading && !selecting)

    onMount(() => {
        title?.focus()
        void open(undefined)
    })
    onDestroy(() => {
        finished = true
    })

    $effect(() => {
        path
        crumbs?.scrollTo({ left: crumbs.scrollWidth })
    })

    $effect(() => {
        const element = sentinel
        const root = list
        if (!element || !root || typeof IntersectionObserver === 'undefined') return
        const observer = new IntersectionObserver(entries => {
            if (entries.some(entry => entry.isIntersecting)) loadMore()
        }, { root })
        observer.observe(element)
        return () => observer.disconnect()
    })

    async function request(token: number, args: { folder?: string; cursor?: string }): Promise<void> {
        lastRequest = args
        loading = true
        loadError = ''
        try {
            const page = await bridge.listFolders({ selectionId, ...args })
            if (finished || token !== navigation) return
            path = page.path
            selectable = page.selectable
            folders = args.cursor ? [...folders, ...page.folders] : page.folders
            nextCursor = page.nextCursor
        } catch (reason) {
            if (finished || token !== navigation) return
            loadError = externalErrorMessage(strings, reason)
        } finally {
            if (token === navigation) loading = false
        }
    }

    function open(folder: ExternalFolderEntry | undefined): Promise<void> {
        const token = ++navigation
        folders = []
        nextCursor = undefined
        selectError = ''
        return request(token, folder ? { folder: folder.handle } : {})
    }

    function loadMore(): void {
        if (!nextCursor || loading || loadError) return
        void request(navigation, { ...(current ? { folder: current.handle } : {}), cursor: nextCursor })
    }

    function retry(): void {
        void request(navigation, lastRequest)
    }

    function onListScroll(): void {
        if (!list || !nextCursor) return
        if (list.scrollTop + list.clientHeight >= list.scrollHeight - 48) loadMore()
    }

    async function select(): Promise<void> {
        if (!current || !canSelect) return
        selecting = true
        selectError = ''
        try {
            const selection = await bridge.selectFolder({ selectionId, folder: current.handle })
            if (finished) return
            finished = true
            onselected(selection)
        } catch (reason) {
            if (finished) return
            selectError = externalErrorMessage(strings, reason)
        } finally {
            selecting = false
        }
    }

    function cancel(): void {
        if (finished || selecting) return
        finished = true
        void bridge.cancelFolderSelection(selectionId).catch(() => {})
        oncancel()
    }

    function onKeydown(event: KeyboardEvent): void {
        if (event.key !== 'Escape') return
        event.preventDefault()
        cancel()
    }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="backdrop fixed inset-0 z-[1300] flex items-center justify-center bg-black/60">
    <div role="dialog" aria-modal="true" aria-labelledby="external-folder-selector-title" data-external-folder-selector class="panel flex w-full flex-col border border-darkborderc bg-darkbg text-textcolor">
        <header class="head">
            <h3 id="external-folder-selector-title" bind:this={title} tabindex="-1" class="title">{strings.selectFolder}</h3>
            <button type="button" class="close" aria-label={strings.close} onclick={cancel}><XIcon size={18} aria-hidden="true" /></button>
        </header>
        <nav class="crumbs" bind:this={crumbs} aria-label={strings.currentFolder}>
            <ol>
                <li><button type="button" class="crumb" aria-current={current ? undefined : 'location'} onclick={() => open(undefined)}>{strings.providers.onedrive.name}</button></li>
                {#each path as entry, index (entry.handle)}
                    <li>
                        <ChevronRightIcon size={14} class="shrink-0 text-subtext" aria-hidden="true" />
                        <button type="button" class="crumb" title={entry.name} aria-current={index === path.length - 1 ? 'location' : undefined} onclick={() => open(entry)}>{entry.name}</button>
                    </li>
                {/each}
            </ol>
        </nav>
        <ul class="list" bind:this={list} aria-busy={loading ? 'true' : undefined} onscroll={onListScroll}>
            {#each folders as entry (entry.handle)}
                <li><button type="button" class="row" onclick={() => open(entry)}><FolderIcon size={18} class="shrink-0 text-subtext" aria-hidden="true" /><span class="name">{entry.name}</span><ChevronRightIcon size={16} class="shrink-0 text-subtext" aria-hidden="true" /></button></li>
            {/each}
            {#if loading}
                <li class="state" role="status">{strings.loading}</li>
            {:else if loadError}
                <li class="state"><span role="alert">{loadError}</span><SettingButton variant="secondary" onclick={retry}>{strings.retryAction}</SettingButton></li>
            {:else if folders.length === 0}
                <li class="state">{strings.noSubfolders}</li>
            {:else if nextCursor}
                <li class="sentinel" bind:this={sentinel} aria-hidden="true"></li>
            {/if}
        </ul>
        {#if selectError}<p class="select-error" role="alert">{selectError}</p>{/if}
        <footer class="foot">
            <SettingButton variant="secondary" disabled={selecting} onclick={cancel}>{strings.cancel}</SettingButton>
            <SettingButton busy={selecting} disabled={!canSelect} onclick={select}>{strings.selectThisFolder}</SettingButton>
        </footer>
    </div>
</div>

<style>
    .backdrop { padding: 1rem; }
    .panel { max-width: 32rem; max-height: min(85vh, 40rem); border-radius: 0.75rem; }
    .head { display: flex; align-items: center; justify-content: space-between; gap: 0.75rem; padding: 1.1rem 1.25rem 0.5rem; }
    .title { margin: 0; font-size: 1.125rem; font-weight: 700; outline: none; }
    .close { display: inline-flex; align-items: center; justify-content: center; width: 2.25rem; height: 2.25rem; border-radius: 0.375rem; color: var(--color-subtext); transition: color 200ms, background-color 200ms; }
    .close:hover { color: var(--risu-theme-textcolor); background: var(--risu-theme-selected); }
    .close:focus-visible, .crumb:focus-visible, .row:focus-visible { outline: none; box-shadow: 0 0 0 2px var(--risu-theme-selected); }
    .crumbs { padding: 0 1.25rem 0.6rem; overflow-x: auto; scrollbar-width: thin; }
    .crumbs ol { display: flex; align-items: center; gap: 0.15rem; margin: 0; padding: 0; list-style: none; white-space: nowrap; }
    .crumbs li { display: inline-flex; align-items: center; gap: 0.15rem; }
    .crumb { max-width: 12rem; padding: 0.25rem 0.4rem; border-radius: 0.375rem; overflow: hidden; text-overflow: ellipsis; font-size: 0.875rem; color: var(--color-subtext); }
    .crumb:hover, .crumb[aria-current] { color: var(--risu-theme-textcolor); }
    .crumb[aria-current] { font-weight: 600; }
    .list { flex: 1 1 auto; min-height: 8rem; margin: 0; padding: 0; list-style: none; overflow-y: auto; border-top: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 55%, transparent); border-bottom: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 55%, transparent); }
    .row { display: flex; align-items: center; gap: 0.6rem; width: 100%; min-height: 2.75rem; padding: 0.55rem 1.25rem; text-align: start; font-size: 0.9375rem; transition: background-color 200ms; }
    .row:hover { background: var(--risu-theme-selected); }
    .row .name { flex: 1 1 auto; min-width: 0; overflow-wrap: anywhere; }
    .state { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem 0.75rem; padding: 0.75rem 1.25rem; font-size: 0.8125rem; line-height: 1.45; color: var(--color-subtext); }
    .state [role='alert'] { color: var(--risu-theme-danger-400); }
    .sentinel { height: 1px; }
    .select-error { margin: 0; padding: 0.6rem 1.25rem 0; font-size: 0.8125rem; line-height: 1.45; color: var(--risu-theme-danger-400); }
    [role='alert'] { white-space: pre-line; overflow-wrap: anywhere; }
    .foot { display: flex; flex-wrap: wrap; justify-content: flex-end; gap: 0.5rem; padding: 0.85rem 1.25rem 1.1rem; }
    @media (max-width: 40rem) {
        .backdrop { padding: 0; align-items: stretch; }
        .panel { max-width: none; height: 100%; max-height: none; border: 0; border-radius: 0; padding-top: env(safe-area-inset-top, 0px); }
        .foot { padding-bottom: calc(1.1rem + env(safe-area-inset-bottom, 0px)); }
        .row { min-height: 3rem; }
    }
</style>
