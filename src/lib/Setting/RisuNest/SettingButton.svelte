<script lang="ts">
    import type { Snippet } from 'svelte'
    import type { HTMLButtonAttributes } from 'svelte/elements'
    import { LoaderCircleIcon } from '@lucide/svelte'
    import { language } from 'src/lang'

    interface Props extends HTMLButtonAttributes {
        /**
         * `primary` is the row's main action, `secondary` an alternative, `danger` a destructive one,
         * and `quiet` a low-emphasis action inside a list row.
         */
        variant?: 'primary' | 'secondary' | 'danger' | 'quiet'
        /** `sm` fits actions inside list rows. */
        size?: 'md' | 'sm'
        /** The action is running: the button is disabled and shows a spinner before its label. */
        busy?: boolean
        children?: Snippet
    }

    let { variant = 'primary', size = 'md', busy = false, disabled = false, type = 'button', class: className = '', children, ...rest }: Props = $props()

    const variants = {
        primary: 'border-darkborderc bg-darkbutton font-medium text-textcolor shadow-xs hover:bg-selected',
        secondary: 'border-darkborderc bg-transparent text-textcolor shadow-xs hover:bg-selected',
        danger: 'danger bg-transparent shadow-xs',
        quiet: 'quiet border-transparent bg-transparent enabled:hover:bg-selected',
    }
    const sizes = {
        md: 'gap-1.5 px-3 py-1.5 text-sm',
        sm: 'gap-1 px-2.5 py-1 text-[13px]',
    }
</script>

<button
    {...rest}
    {type}
    disabled={disabled || busy}
    aria-busy={busy ? 'true' : undefined}
    class="inline-flex items-center justify-center rounded-md border whitespace-nowrap transition-colors duration-200 focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected disabled:cursor-not-allowed disabled:opacity-50 {sizes[size]} {variants[variant]} {className}"
>
    {#if busy}
        <LoaderCircleIcon size={size === 'sm' ? 13 : 14} class="shrink-0 motion-safe:animate-spin" aria-hidden="true" />
    {/if}
    {@render children?.()}
    {#if busy}
        <span class="sr-only">{language.loading}</span>
    {/if}
</button>

<style>
    /* Mixed toward the text color so the red stays readable on light and dark themes alike. */
    .danger {
        border-color: color-mix(in srgb, var(--risu-theme-danger-400) 55%, transparent);
        color: color-mix(in srgb, var(--risu-theme-danger-400) 65%, var(--risu-theme-textcolor));
    }
    .danger:not(:disabled):hover {
        background: color-mix(in srgb, var(--risu-theme-danger-400) 12%, transparent);
    }
    /* Lifted above secondary text (subtext) so an enabled quiet action reads as one, and faded further when disabled. */
    .quiet {
        color: color-mix(in srgb, var(--risu-theme-textcolor) 60%, var(--risu-theme-textcolor2));
    }
    .quiet:not(:disabled):hover {
        color: var(--risu-theme-textcolor);
    }
    .quiet:disabled {
        color: var(--risu-theme-textcolor2);
        opacity: 0.45;
    }
</style>
