<script lang="ts">
    import { untrack } from 'svelte'
    import ChatScreenshotDialog from './ChatScreenshotDialog.svelte'

    interface Props {
        onCancel: () => void
        initialRunning?: boolean
        initialTotalTurns?: number
    }

    let { onCancel, initialRunning = true, initialTotalTurns = 10 }: Props = $props()
    let visible = $state(true)
    let running = $state(untrack(() => initialRunning))
    let totalTurns = $state(untrack(() => initialTotalTurns))

    export function destroyDialog() {
        visible = false
    }

    export function setRunning(next: boolean) {
        running = next
    }

    export function setTotalTurns(next: number) {
        totalTurns = next
    }
</script>

{#if visible}
    <ChatScreenshotDialog
        {totalTurns}
        {running}
        onStart={() => {}}
        {onCancel}
        onClose={() => {}}
    />
{/if}
