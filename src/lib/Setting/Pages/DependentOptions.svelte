<script lang="ts">
    import type { Snippet } from 'svelte'

    // Options that apply only while their parent option is on are hidden while it is off, keeping their values.
    // Set to false to show them at all times, still indented under the parent.
    const hiddenWhileParentOff = true

    interface Props {
        parentOn: boolean | undefined
        children: Snippet
    }

    let { parentOn, children }: Props = $props()
</script>
{#if parentOn || !hiddenWhileParentOff}
    <div class="ml-2.5 pl-4 border-l-2 border-selected flex flex-col">
        {@render children()}
    </div>
{/if}
