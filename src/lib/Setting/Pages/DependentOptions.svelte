<script lang="ts">
    import type { Snippet } from 'svelte'

    // Options that apply only while their parent option is on are hidden while it is off, keeping their values.
    // The parent's label carries DependentOptionsHint, which tells that they are there.
    interface Props {
        parentOn: boolean | undefined
        children: Snippet
    }

    let { parentOn, children }: Props = $props()
</script>
{#if parentOn}
    <div class="ml-2.5 pl-4 border-l-2 border-selected flex flex-col">
        {@render children()}
    </div>
{/if}
