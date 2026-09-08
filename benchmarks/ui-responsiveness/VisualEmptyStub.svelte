<script lang="ts">
    export let close: (() => void) | undefined = undefined
</script>

{#if close}
    <button onclick={close}>Close synthetic panel</button>
{/if}
