<script lang="ts">
    import CaseSensitiveIcon from '@lucide/svelte/icons/case-sensitive'
    import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
    import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
    import RegexIcon from '@lucide/svelte/icons/regex'
    import SearchIcon from '@lucide/svelte/icons/search'
    import Undo2Icon from '@lucide/svelte/icons/undo-2'
    import XIcon from '@lucide/svelte/icons/x'
    import { language } from 'src/lang'
    import { isCompositionKey } from 'src/ts/hotkeyModifier'
    import { backNavigationLayer } from 'src/ts/ui/modalNavigation'

    interface Props {
        query: string
        replacement: string
        matchCase: boolean
        regex: boolean
        showReplace: boolean
        invalid: boolean
        /** Shown while a query is active. */
        counts: { matches: number, blocks: number } | null
        canReplace: boolean
        /** The result of the last replace, shown with 되돌리기. */
        status: string
        onReplaceAll: () => void
        onUndo: () => void
        onClose: () => void
        /** Enter in the search input. */
        onSubmit: () => void
    }

    let {
        query = $bindable(),
        replacement = $bindable(),
        matchCase = $bindable(),
        regex = $bindable(),
        showReplace = $bindable(),
        invalid,
        counts,
        canReplace,
        status,
        onReplaceAll,
        onUndo,
        onClose,
        onSubmit,
    }: Props = $props()

    let queryInput: HTMLInputElement | undefined = $state()
    const errorId = $props.id()
    const text = language.risuNest.promptSearch
    const iconButton = 'flex items-center justify-center rounded-md text-textcolor2 hover:text-textcolor hover:bg-selected transition-colors'
    const input = 'border peer rounded-md shadow-xs text-textcolor bg-transparent focus:ring-2 focus:outline-hidden transition-colors duration-200'

    export function focusQuery() {
        queryInput?.focus()
        queryInput?.select()
    }
</script>

<div class="@container/search sticky top-0 z-50 bg-bgcolor px-3 pt-1 pb-2 flex flex-col gap-1.5" role="search" use:backNavigationLayer={{ close: onClose }}>
    <div class="flex flex-wrap items-center gap-1.5">
        <button class="size-8 {iconButton}" aria-expanded={showReplace} aria-label={text.replace} title={text.replace} onclick={() => {
            showReplace = !showReplace
        }}>
            {#if showReplace}<ChevronDownIcon size={16} />{:else}<ChevronRightIcon size={16} />{/if}
        </button>
        <label class="relative flex-1 min-w-40">
            <span class="sr-only">{language.search}</span>
            <SearchIcon size={16} class="absolute left-2.5 top-1/2 -translate-y-1/2 text-textcolor2 pointer-events-none" />
            <input
                bind:this={queryInput}
                bind:value={query}
                class="{input} {invalid ? 'border-draculared focus:border-draculared focus:ring-draculared' : 'border-darkborderc focus:border-borderc focus:ring-borderc'} w-full pl-8 pr-[4.25rem] py-1.5 text-sm"
                placeholder={language.search}
                aria-label={language.search}
                aria-invalid={invalid}
                aria-describedby={invalid ? errorId : undefined}
                onkeydown={(event) => {
                    if (event.key !== 'Enter' || isCompositionKey(event)) return
                    event.preventDefault()
                    onSubmit()
                }}
            >
            <span class="absolute right-1 top-1/2 -translate-y-1/2 flex gap-0.5">
                <button class="size-7 {iconButton}" class:bg-selected={matchCase} class:text-textcolor={matchCase} aria-pressed={matchCase} aria-label={text.matchCase} title={text.matchCase} onclick={() => {
                    matchCase = !matchCase
                }}><CaseSensitiveIcon size={16} /></button>
                <button class="size-7 {iconButton}" class:bg-selected={regex} class:text-textcolor={regex} aria-pressed={regex} aria-label={language.regex} title={language.regex} onclick={() => {
                    regex = !regex
                }}><RegexIcon size={16} /></button>
            </span>
        </label>
        {#if counts}
            <span class="shrink-0 text-sm text-subtext tabular-nums order-last basis-full pl-[2.375rem] @lg/search:order-none @lg/search:basis-auto @lg/search:pl-0">{text.matchCount.replace('{0}', String(counts.matches))} · {language.risuNest.promptList.count.replace('{0}', String(counts.blocks))}</span>
        {/if}
        <button class="size-8 {iconButton}" aria-label={language.close} title={language.close} onclick={onClose}><XIcon size={16} /></button>
    </div>
    {#if invalid}
        <p class="pl-[2.375rem] text-xs text-draculared" id={errorId}>{text.invalidRegex}</p>
    {/if}
    {#if showReplace}
        <div class="flex items-center gap-1.5 pl-[2.375rem] pr-[2.375rem]">
            <input class="{input} border-darkborderc focus:border-borderc focus:ring-borderc flex-1 min-w-0 px-2.5 py-1.5 text-sm" placeholder={text.replaceWith} aria-label={text.replaceWith} bind:value={replacement}>
            <button class="bg-darkbutton hover:bg-selected focus:ring-selected border-darkborderc border text-textcolor rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 inline-flex items-center gap-1.5 shrink-0 px-3 py-1.5 text-sm disabled:opacity-50 disabled:cursor-not-allowed" disabled={!canReplace} onclick={onReplaceAll}>{text.replaceAll}</button>
        </div>
    {/if}
    {#if status}
        <div class="flex items-center gap-2 pl-[2.375rem] text-sm text-textcolor2">
            <span>{status}</span>
            <button class="bg-transparent hover:bg-darkbg focus:ring-selected border-darkborderc text-textcolor2 border rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 inline-flex items-center gap-1.5 px-2 py-0.5 text-xs" onclick={onUndo}><Undo2Icon size={12} />{text.undo}</button>
        </div>
    {/if}
</div>
