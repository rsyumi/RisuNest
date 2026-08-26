<script lang="ts">
    import { onMount } from 'svelte'

    let {
        msgDisplay = '',
        onCaptureSettled,
    }: {
        msgDisplay?: string
        onCaptureSettled?: (generation: number) => void
    } = $props()

    onMount(() => onCaptureSettled?.(1))
</script>

<span data-chat-body-probe>{msgDisplay}</span>
