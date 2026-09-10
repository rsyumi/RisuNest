<script lang="ts">
    import type { Snippet } from 'svelte'
    import type { HTMLAttributes } from 'svelte/elements'

    interface Props extends HTMLAttributes<HTMLDivElement> {
        label?: string
        help?: string
        /** Id of the control the label describes; renders a real label element. */
        labelFor?: string
        /** Extra content under the help text, such as a status line. */
        below?: Snippet
        /** The control, aligned to the right on wide panels. */
        children?: Snippet
    }

    let { label, help, labelFor, below, children, class: className = '', ...rest }: Props = $props()
</script>

<div {...rest} class="grid grid-cols-1 items-center gap-x-6 gap-y-2 px-4 py-3 @xl:grid-cols-[minmax(0,1fr)_auto] {className}">
    <div class="min-w-0">
        {#if label}
            {#if labelFor}
                <label class="text-[15px]" for={labelFor}>{label}</label>
            {:else}
                <div class="text-[15px]">{label}</div>
            {/if}
        {/if}
        {#if help}
            <p class="help mt-0.5 max-w-[62ch] text-[13px] leading-normal">{help}</p>
        {/if}
        {@render below?.()}
    </div>
    {#if children}
        <div class="flex flex-wrap items-center gap-2 @xl:justify-end @xl:justify-self-end">{@render children()}</div>
    {/if}
</div>

<style>
    .help {
        color: color-mix(in srgb, var(--risu-theme-textcolor2) 62%, var(--risu-theme-textcolor) 38%);
    }
</style>
