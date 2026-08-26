<script lang="ts">
    import { onMount } from 'svelte'

    let {
        message = '',
        idx = -1,
        onCaptureSettled,
    }: {
        message?: string
        idx?: number
        onCaptureSettled?: (generation: number) => void
    } = $props()

    onMount(() => {
        if (message !== 'pending') onCaptureSettled?.(1)
    })
</script>

<div data-capture-probe={message} data-index={idx}>{message}</div>
