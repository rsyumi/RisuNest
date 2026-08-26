<script lang="ts">
    import type { Snippet } from 'svelte'

    let {
        children,
        onclick,
    }: {
        children?: Snippet
        onclick?: (event: MouseEvent) => void
    } = $props()
</script>

<button {onclick}>
    {@render children?.()}
</button>
