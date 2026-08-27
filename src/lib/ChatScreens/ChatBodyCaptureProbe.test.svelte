<script lang="ts">
    import { onMount } from 'svelte'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'

    let {
        msgDisplay = '',
        onCaptureSettled,
        parserProjection,
    }: {
        msgDisplay?: string
        onCaptureSettled?: (generation: number) => void
        parserProjection?: BoundedLiveChatParserProjection
    } = $props()

    onMount(() => onCaptureSettled?.(1))
</script>

<span
    data-chat-body-probe
    data-parser-projection={parserProjection?.kind ?? ''}
    data-projected-chat-id={parserProjection?.projectedChatID}
>{msgDisplay}</span>
