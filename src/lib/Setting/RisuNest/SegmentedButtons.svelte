<script lang="ts" generics="T extends string | number">
    interface Props {
        value: T
        options: { value: T; label: string }[]
        /** Accessible name of the whole control. */
        label: string
        /** `group` renders pressed buttons; `radiogroup` renders radio semantics. */
        role?: 'group' | 'radiogroup'
        /** Ignore clicks and dim the control while the owning row is busy. */
        disabled?: boolean
        onchange?: (value: T) => void
    }

    let { value = $bindable(), options, label, role = 'group', disabled = false, onchange }: Props = $props()

    function select(next: T): void {
        if (disabled || next === value) return
        value = next
        onchange?.(next)
    }
</script>

<!-- Options share the width equally and wrap their own text on narrow panels, so the control never breaks into rows. -->
<div
    {role}
    aria-label={label}
    aria-disabled={disabled ? 'true' : undefined}
    class="grid w-full grid-flow-col auto-cols-fr gap-1 rounded-lg border border-darkborderc bg-bgcolor p-1 @xl:inline-grid @xl:w-auto {disabled ? 'opacity-50' : ''}"
>
    {#each options as option (option.value)}
        {@const active = option.value === value}
        <button
            type="button"
            role={role === 'radiogroup' ? 'radio' : undefined}
            aria-checked={role === 'radiogroup' ? active : undefined}
            aria-pressed={role === 'group' ? active : undefined}
            {disabled}
            class="min-w-0 rounded-md px-3 py-1.5 text-center text-sm leading-snug transition-colors duration-200 focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected disabled:cursor-not-allowed @xl:whitespace-nowrap {active ? 'bg-darkbutton font-medium text-textcolor shadow-xs ring-1 ring-darkborderc' : 'text-subtext hover:bg-selected/50 hover:text-textcolor'}"
            onclick={() => select(option.value)}
        >{option.label}</button>
    {/each}
</div>
