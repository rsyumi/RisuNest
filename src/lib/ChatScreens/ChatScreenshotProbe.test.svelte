<script lang="ts">
    import { onMount } from 'svelte'

    let {
        item,
        context,
        onCaptureSettled,
        onCaptureError,
    }: {
        item: { turn: number; message: { data: string; time?: number } }
        context: { characterName: string }
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
    } = $props()

    onMount(() => {
        if (item.message.data === 'error') onCaptureError?.(1, new Error('parse failed'))
        else if (item.message.data !== 'pending') onCaptureSettled?.(1)
    })
</script>

<div data-capture-probe={item.message.data} data-index={item.turn - 1}>{item.message.data}</div>
<span data-character-name>{context.characterName}</span>
<span data-message-time>{item.message.time}</span>
