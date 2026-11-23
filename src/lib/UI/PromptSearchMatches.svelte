<script module lang="ts">
    export interface PromptSearchLine {
        label: string
        before: string
        hit: string
        after: string
        onPick: () => void
        /** Present while the replace line is shown. */
        onReplace?: () => void
    }
</script>

<script lang="ts">
    import { language } from 'src/lang'

    interface Props {
        lines: PromptSearchLine[]
        /** A new value starts the list over at the first lines. */
        resetKey: string
    }

    let { lines, resetKey }: Props = $props()

    const FIRST = 3
    const MORE = 20
    let expanded = $state({ key: '', count: FIRST })
    const limit = $derived(expanded.key === resetKey ? expanded.count : FIRST)
</script>

{#if lines.length > 0}
    <div class="flex flex-col border-t border-darkborderc py-1">
        {#each lines.slice(0, limit) as line, index (index)}
            <div class="flex items-center gap-2 pl-3 pr-1 hover:bg-selected/40">
                <button class="min-w-0 flex-1 py-2 text-left text-sm flex items-baseline gap-2" data-prompt-search-line onclick={line.onPick}>
                    <span class="shrink-0 w-16 text-xs text-subtext">{line.label}</span>
                    <span class="truncate text-textcolor2">{line.before}{#if line.hit}<mark class="rounded-xs bg-primary-500/35 text-textcolor px-0.5">{line.hit}</mark>{/if}{line.after}</span>
                </button>
                {#if line.onReplace}
                    <button class="bg-transparent hover:bg-darkbg focus:ring-selected border-darkborderc text-textcolor2 border rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 inline-flex items-center shrink-0 px-2 py-1 text-xs" onclick={line.onReplace}>{language.risuNest.promptSearch.replace}</button>
                {/if}
            </div>
        {/each}
        {#if lines.length > limit}
            <button class="self-start pl-3 py-1.5 text-xs text-textcolor2 hover:text-textcolor" onclick={() => {
                expanded = { key: resetKey, count: limit + MORE }
            }}>{language.risuNest.promptSearch.showMore.replace('{0}', String(Math.min(MORE, lines.length - limit)))}</button>
        {/if}
    </div>
{/if}
