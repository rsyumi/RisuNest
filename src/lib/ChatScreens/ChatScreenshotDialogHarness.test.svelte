<script lang="ts">
    import ChatScreenshotDialog from './ChatScreenshotDialog.svelte'

    let { onCancel }: { onCancel: () => void } = $props()
    let visible = $state(true)

    export function destroyDialog() {
        visible = false
    }
</script>

{#if visible}
    <ChatScreenshotDialog
        totalTurns={10}
        running={true}
        onStart={() => {}}
        {onCancel}
        onClose={() => {}}
    />
{/if}
