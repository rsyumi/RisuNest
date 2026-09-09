<script lang="ts">
    import { ArrowLeft, ArrowRight, RefreshCcwIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    let {
        currentPage = 1,
        totalPages = 1,
        greeting = false,
        showPages = true,
        dynamic = false,
        busy = false,
        previous,
        next,
        generate,
    }: {
        currentPage?: number
        totalPages?: number
        greeting?: boolean
        showPages?: boolean
        dynamic?: boolean
        busy?: boolean
        previous: () => void
        next: () => void
        generate: () => void
    } = $props()
</script>

<button
    disabled={busy || (!greeting && currentPage <= 1)}
    title={language.previousResponseCandidate}
    class="flex items-center p-1 hover:text-blue-500 disabled:opacity-30 transition-colors button-icon-unreroll"
    class:dyna-icon={dynamic}
    onclick={previous}><ArrowLeft size={22} /></button
>
{#if showPages}
    <span
        class="flex items-center text-xs text-textcolor2 tabular-nums"
        class:dyna-icon={dynamic}
        aria-label={language.responseCandidates}>{currentPage}/{totalPages}</span
    >
{/if}
<button
    disabled={busy}
    title={language.nextResponseCandidate}
    class="flex items-center p-1 hover:text-blue-500 disabled:opacity-30 transition-colors"
    class:button-icon-reroll={greeting}
    class:dyna-icon={dynamic}
    onclick={next}><ArrowRight size={22} /></button
>
{#if !greeting}
    <button
        disabled={busy}
        title={language.newResponseCandidate}
        class="flex items-center p-1 hover:text-blue-500 disabled:opacity-30 transition-colors button-icon-reroll"
        class:dyna-icon={dynamic}
        onclick={generate}><RefreshCcwIcon size={20} /></button
    >
{/if}
