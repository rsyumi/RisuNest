<script lang="ts">
    import type { Snippet } from 'svelte'
    let { children, chara = $bindable() }: { children?: Snippet; chara?: unknown } = $props()
</script>

<section data-navigation-panel>{@render children?.()}</section>
