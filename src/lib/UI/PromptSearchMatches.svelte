<script module lang="ts">
    export interface PromptSearchLine {
        label: string
        before: string
        hit: string
        after: string
        /** The line starts earlier than `before` does. */
        cut: boolean
        onPick: () => void
        /** Present while the replace line is shown. */
        onReplace?: () => void
    }
</script>

<script lang="ts">
    import { language } from 'src/lang'

    interface Props {
        count: number
        /** Builds the line at `index`; only the lines shown are built. */
        line: (index: number) => PromptSearchLine
        /** A new value starts the list over at the first lines. */
        resetKey: string
    }

    let { count, line, resetKey }: Props = $props()

    const FIRST = 3
    const MORE = 20
    let expanded = $state({ key: '', count: FIRST })
    const limit = $derived(expanded.key === resetKey ? expanded.count : FIRST)
    const shown = $derived(Array.from({ length: Math.min(count, limit) }, (_, index) => index))

    /** Shows the ellipsis ahead of the text before the match while that text is cut, by the line or by the width of its box. */
    function cutStart(row: HTMLElement, cut: boolean) {
        const [ellipsis, box] = row.children as unknown as [HTMLElement, HTMLElement]
        const update = () => {
            const content = box.firstElementChild
            const clipped = !!content && content.getBoundingClientRect().width - box.getBoundingClientRect().width > 0.5
            ellipsis.hidden = !(cut || clipped)
            // A character the box cuts partway fades in rather than showing as a sliver.
            box.style.maskImage = clipped ? 'linear-gradient(to right, transparent, black 0.7em)' : ''
        }
        // The row's width does not depend on the ellipsis, so showing it does not resize what is observed.
        const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(update)
        observer?.observe(row)
        if (box.firstElementChild) observer?.observe(box.firstElementChild)
        update()
        return {
            update(next: boolean) {
                cut = next
                update()
            },
            destroy() {
                observer?.disconnect()
            },
        }
    }
</script>

{#if count > 0}
    <div class="flex flex-col border-t border-darkborderc py-1">
        {#each shown as index (index)}
            {@const item = line(index)}
            <div class="flex items-center gap-2 pl-3 pr-1 hover:bg-selected/40">
                <button class="min-w-0 flex-1 py-2 text-left text-sm flex items-baseline gap-2" data-prompt-search-line onclick={item.onPick}>
                    <span class="shrink-0 w-16 text-xs text-subtext">{item.label}</span>
                    <!-- The text before the match gives way from its start, so the match stays in view at any width. -->
                    <span class="min-w-0 flex-1 flex items-baseline overflow-hidden whitespace-pre text-textcolor2" use:cutStart={item.cut}><span class="shrink-0 before:content-['…']" aria-hidden="true"></span><span class="min-w-0 max-w-[45%] shrink overflow-hidden flex justify-end"><span class="shrink-0">{item.before}</span></span>{#if item.hit}<mark class="shrink-0 max-w-full overflow-hidden text-ellipsis rounded-xs bg-primary-500/35 text-textcolor px-0.5">{item.hit}</mark>{/if}<span class="min-w-0 flex-1 overflow-hidden text-ellipsis">{item.after}</span></span>
                </button>
                {#if item.onReplace}
                    <button class="bg-transparent hover:bg-darkbg focus:ring-selected border-darkborderc text-textcolor2 border rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 inline-flex items-center shrink-0 px-2 py-1 text-xs" onclick={item.onReplace}>{language.risuNest.promptSearch.replace}</button>
                {/if}
            </div>
        {/each}
        {#if count > limit}
            <button class="self-start pl-3 py-1.5 text-xs text-textcolor2 hover:text-textcolor" onclick={() => {
                expanded = { key: resetKey, count: limit + MORE }
            }}>{language.risuNest.promptSearch.showMore.replace('{0}', String(Math.min(MORE, count - limit)))}</button>
        {/if}
    </div>
{/if}
