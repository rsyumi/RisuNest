<script lang="ts">
    import type { Snippet } from 'svelte'

    interface Props {
        check?: boolean
        name: string
        disabled?: boolean
        /** Tints the row, used for toggles whose value differs from the chat binding. */
        highlight?: boolean
        onChange?: (checked: boolean) => void
        children?: Snippet
    }

    let { check = $bindable(), name, disabled = false, highlight = false, onChange, children }: Props = $props()
    const id = $props.id()
</script>

<div
    class="w-full flex gap-2 mt-2 items-center justify-between min-h-10 rounded-md px-1 transition-colors {highlight
        ? 'bg-draculared/15'
        : ''}"
>
    <div class="min-w-0 break-words">
        <label for={id} class="cursor-pointer">{name}</label>
        {@render children?.()}
    </div>
    <label class="shrink-0 inline-flex h-10 w-10 items-center justify-end cursor-pointer">
        <input
            {id}
            type="checkbox"
            role="switch"
            class="peer sr-only"
            bind:checked={check}
            {disabled}
            onchange={() => onChange?.(check)}
        />
        <span
            aria-hidden="true"
            class="inline-flex h-[18.4px] w-8 items-center rounded-full border border-transparent shadow-xs transition-colors peer-focus-visible:ring-2 peer-focus-visible:ring-borderc peer-focus-visible:ring-offset-2 peer-focus-visible:ring-offset-bgcolor peer-disabled:opacity-50 peer-disabled:cursor-not-allowed {check
                ? 'bg-primary-500'
                : 'bg-darkbutton'}"
        >
            <span
                class="size-4 rounded-full bg-white shadow-xs transition-transform {check
                    ? 'translate-x-[14px] rtl:-translate-x-[14px]'
                    : 'translate-x-0'}"
            ></span>
        </span>
    </label>
</div>
