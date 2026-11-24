<script lang="ts">
    import { untrack } from 'svelte'
    import ChatScreenshotDialog from './ChatScreenshotDialog.svelte'
    import { provideChatWindowLayer } from './ChatWindowLayer.svelte'

    interface Props {
        onCancel: () => void
        initialRunning?: boolean
        initialTotalTurns?: number
        windowLayer?: HTMLElement
    }

    let { onCancel, initialRunning = true, initialTotalTurns = 10, windowLayer }: Props = $props()
    // svelte-ignore state_referenced_locally
    if (windowLayer) provideChatWindowLayer(() => windowLayer)
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
