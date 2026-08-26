<script lang="ts">
    import { onMount } from 'svelte'

    let {
        message,
        idx,
        captureMessage,
        captureContext,
        captureParserIndex,
        onCaptureSettled,
        onCaptureError,
    }: {
        message: string
        idx: number
        captureMessage: { data: string; time?: number }
        captureContext: { characterName: string }
        captureParserIndex: number
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
    } = $props()

    onMount(() => {
        if (message === 'error') onCaptureError?.(1, new Error('parse failed'))
        else if (message !== 'pending') onCaptureSettled?.(1)
    })
</script>

<div data-capture-probe={message} data-index={idx} data-parser-index={captureParserIndex}>{message}</div>
<span data-character-name>{captureContext.characterName}</span>
<span data-message-time>{captureMessage.time}</span>
