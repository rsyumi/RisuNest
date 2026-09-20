<script lang="ts">
    interface Props {
        checked?: boolean
        label: string
        /** Show the label beside the box instead of exposing it only to assistive technology. */
        showLabel?: boolean
        /** Some, but not all, of the items this box stands for are chosen. */
        indeterminate?: boolean
        disabled?: boolean
        id?: string
        onchange?: (checked: boolean) => void
    }

    let { checked = $bindable(false), label, showLabel = false, indeterminate = false, disabled = false, id, onchange }: Props = $props()
</script>

<label class="inline-flex items-center gap-2 rounded-md text-textcolor focus-within:outline focus-within:outline-2 focus-within:outline-darkborderc focus-within:outline-offset-2 {disabled ? 'cursor-not-allowed opacity-50' : 'cursor-pointer'}">
    <input {id} class="sr-only" type="checkbox" bind:checked {indeterminate} {disabled} onchange={(event) => onchange?.(event.currentTarget.checked)} />
    <span class="flex h-5 w-5 min-h-5 min-w-5 items-center justify-center rounded-md border-2 border-darkborderc transition-colors duration-200 {checked || indeterminate ? 'bg-darkborderc' : 'bg-darkbutton'}" aria-hidden="true">
        {#if checked}
            <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke="white" class="h-3 w-3" aria-hidden="true">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7"/>
            </svg>
        {:else if indeterminate}
            <span class="h-0.5 w-2.5 rounded-full bg-white"></span>
        {/if}
    </span>
    <span class="text-sm" class:sr-only={!showLabel}>{label}</span>
</label>
