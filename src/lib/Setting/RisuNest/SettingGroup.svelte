<script lang="ts">
    import type { Snippet } from 'svelte'

    interface Props {
        title: string
        id?: string
        /** One sentence under the heading, before the panel. */
        description?: string
        /** Rendered on the right side of the heading row. */
        actions?: Snippet
        /** Attributes applied to the panel element, such as test hooks. */
        panelProps?: Record<string, string | undefined>
        /** Separate the panel's direct children with hairlines. */
        divide?: boolean
        children?: Snippet
    }

    let { title, id, description, actions, panelProps = {}, divide = true, children }: Props = $props()
</script>

<section {id} class="@container mt-7 scroll-mt-3">
    <div class="mb-2 flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-0.5">
        <h2 class="text-lg font-bold">{title}</h2>
        {#if actions}
            <div class="ml-auto flex flex-wrap items-center gap-2">{@render actions()}</div>
        {/if}
    </div>
    {#if description}
        <p class="mb-2.5 px-0.5 text-[13px] leading-normal text-textcolor/70">{description}</p>
    {/if}
    <div {...panelProps} class="rounded-lg border border-darkborderc bg-darkbg {divide ? 'divide-y divide-darkborderc/55' : ''}">
        {@render children?.()}
    </div>
</section>
