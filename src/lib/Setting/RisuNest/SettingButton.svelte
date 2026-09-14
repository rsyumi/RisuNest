<script lang="ts">
    import type { Snippet } from 'svelte'
    import type { HTMLButtonAttributes } from 'svelte/elements'
    import { LoaderCircleIcon } from '@lucide/svelte'
    import { language } from 'src/lang'

    interface Props extends HTMLButtonAttributes {
        /** `primary` is the row's main action, `secondary` an alternative, `danger` a destructive one. */
        variant?: 'primary' | 'secondary' | 'danger'
        /** The action is running: the button is disabled and shows a spinner before its label. */
        busy?: boolean
        children?: Snippet
    }

    let { variant = 'primary', busy = false, disabled = false, type = 'button', class: className = '', children, ...rest }: Props = $props()

    const variants = {
        primary: 'border-darkborderc bg-darkbutton text-textcolor hover:bg-selected',
        secondary: 'border-darkborderc bg-transparent text-textcolor2 hover:bg-selected hover:text-textcolor',
        danger: 'border-danger-400/50 bg-transparent text-danger-400 hover:bg-danger-400/10',
    }
</script>

<button
    {...rest}
    {type}
    disabled={disabled || busy}
    aria-busy={busy ? 'true' : undefined}
    class="inline-flex items-center justify-center gap-1.5 rounded-md border px-3 py-1.5 text-sm whitespace-nowrap shadow-xs transition-colors duration-200 focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected disabled:cursor-not-allowed disabled:opacity-50 {variants[variant]} {className}"
>
    {#if busy}
        <LoaderCircleIcon size={14} class="shrink-0 motion-safe:animate-spin" aria-hidden="true" />
    {/if}
    {@render children?.()}
    {#if busy}
        <span class="sr-only">{language.loading}</span>
    {/if}
</button>
