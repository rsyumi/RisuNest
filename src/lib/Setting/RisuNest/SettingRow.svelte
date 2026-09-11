<script lang="ts">
    import type { Snippet } from 'svelte'
    import type { HTMLAttributes } from 'svelte/elements'

    interface Props extends HTMLAttributes<HTMLDivElement> {
        label?: string
        help?: string
        /** Id of the control the label describes; renders a real label element. */
        labelFor?: string
        /**
         * Keep the control on the label's line even on narrow panels, with the
         * help text under both. Meant for small controls such as toggles; wide
         * controls stack below the help instead.
         */
        inline?: boolean
        /** Extra content under the help text, such as a status line. */
        below?: Snippet
        /** The control, aligned to the right on wide panels. */
        children?: Snippet
    }

    let { label, help, labelFor, inline = false, below, children, class: className = '', ...rest }: Props = $props()
</script>

<div {...rest} class="grid items-center gap-x-6 px-4 py-3 {inline ? 'grid-cols-[minmax(0,1fr)_auto] gap-y-0' : 'grid-cols-1 gap-y-2 @xl:grid-cols-[minmax(0,1fr)_auto]'} {className}">
    <div class="min-w-0">
        {#if label}
            {#if labelFor}
                <label class="text-[15px]" for={labelFor}>{label}</label>
            {:else}
                <div class="text-[15px]">{label}</div>
            {/if}
        {/if}
        {#if !inline}
            {#if help}
                <p class="help mt-0.5 max-w-[62ch] text-[13px] leading-normal">{help}</p>
            {/if}
            {@render below?.()}
        {/if}
    </div>
    {#if children}
        <div class="flex flex-wrap items-center gap-2 {inline ? 'justify-end justify-self-end @xl:row-span-2' : '@xl:justify-end @xl:justify-self-end'}">{@render children()}</div>
    {/if}
    {#if inline && (help || below)}
        <div class="col-span-2 min-w-0 @xl:col-span-1">
            {#if help}
                <p class="help mt-0.5 max-w-[62ch] text-[13px] leading-normal">{help}</p>
            {/if}
            {@render below?.()}
        </div>
    {/if}
</div>

<style>
    .help {
        color: color-mix(in srgb, var(--risu-theme-textcolor2) 62%, var(--risu-theme-textcolor) 38%);
    }
</style>
