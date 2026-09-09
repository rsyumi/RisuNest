<script lang="ts">
    import { onMount } from 'svelte'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import type { StreamingThoughtPreview } from '../../ts/parser/streamingThoughtPreview'

    let {
        msgDisplay = '',
        onCaptureSettled,
        parserProjection,
        thoughtPreview = null,
        streamingThoughtMode = 'off',
        renderRawStreaming = false,
    }: {
        msgDisplay?: string
        onCaptureSettled?: (generation: number) => void
        parserProjection?: BoundedLiveChatParserProjection
        thoughtPreview?: StreamingThoughtPreview | null
        streamingThoughtMode?: string
        renderRawStreaming?: boolean
    } = $props()

    onMount(() => onCaptureSettled?.(1))
</script>

<span
    data-chat-body-probe
    data-parser-projection={parserProjection?.kind ?? ''}
    data-projected-chat-id={parserProjection?.projectedChatID}
    data-thought-preview={thoughtPreview !== null}
    data-thought-mode={streamingThoughtMode}
    data-raw-preview={renderRawStreaming}
    >{thoughtPreview?.recent ?? msgDisplay}</span
>
