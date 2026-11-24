<script lang="ts">
    import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
    import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'

    // Goes inside the label of an option that has DependentOptions under it, so a press on it is a press on the option.
    interface Props {
        parentOn: boolean | undefined
    }

    let { parentOn }: Props = $props()
</script>
{#if parentOn}<ChevronDownIcon size={16} class="inline-block align-text-bottom text-textcolor2" aria-hidden="true" />{:else}<ChevronRightIcon size={16} class="inline-block align-text-bottom text-textcolor2" aria-hidden="true" />{/if}
