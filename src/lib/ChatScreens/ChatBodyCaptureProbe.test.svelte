<script module lang="ts">
    import type { DisplayRowReads } from 'src/ts/process/scriptVariableReads'

    /** The row registrations the probed bodies received, newest last. */
    export const probedDisplayRows: DisplayRowReads[] = []
</script>

<script lang="ts">
    import { onMount, settled as domSettled, untrack } from 'svelte'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import type { StreamingThoughtPreview } from '../../ts/parser/streamingThoughtPreview'

    let {
        msgDisplay = '',
        onCaptureSettled,
        onDisplaySettled,
        parserProjection,
        thoughtPreview = null,
        streamingThoughtMode = 'off',
        renderRawStreaming = false,
        displayRow,
    }: {
        msgDisplay?: string
        onCaptureSettled?: (generation: number) => void
        onDisplaySettled?: () => void
        parserProjection?: BoundedLiveChatParserProjection
        thoughtPreview?: StreamingThoughtPreview | null
        streamingThoughtMode?: string
        renderRawStreaming?: boolean
        displayRow?: DisplayRowReads
    } = $props()
    untrack(() => displayRow && probedDisplayRows.push(displayRow))

    onMount(() => onCaptureSettled?.(1))
    $effect(() => {
        const settled = onDisplaySettled
        void domSettled().then(() => settled?.())
    })
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
