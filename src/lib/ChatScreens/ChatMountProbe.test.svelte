<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import type { StreamingDisplayOptimizationMode } from 'src/ts/storage/database.svelte'
    import { chatMountProbe } from './chatMountProbe'

    let {
        message,
        idx,
        img,
        character,
        rawStreamingText,
        bookmarked = false,
    }: {
        message: string
        idx: number
        img: string
        character: unknown
        rawStreamingText: string
        bookmarked?: boolean
    } = $props()

    const instanceId = chatMountProbe.nextInstanceId++
    let displayedStreamingText = $state('')

    export function updateStreamingDisplay(state: {
        isOptimizedStreamingMessage: boolean
        streamingOptimizationMode: StreamingDisplayOptimizationMode
        rawStreamingText: string
    }) {
        displayedStreamingText = state.rawStreamingText
        chatMountProbe.streamingUpdates.push({
            instanceId,
            rawStreamingText: state.rawStreamingText,
            isOptimizedStreamingMessage: state.isOptimizedStreamingMessage,
        })
    }

    onMount(() => {
        displayedStreamingText = rawStreamingText
        chatMountProbe.mounts.push({ instanceId, message, index: idx, image: img, character, bookmarked })
    })

    onDestroy(() => {
        chatMountProbe.unmounts.push(instanceId)
    })
</script>

<div
    data-chat-probe={instanceId}
    data-message={message}
    data-index={idx}
    data-image={img}
    data-streaming-text={displayedStreamingText}
    data-bookmarked={bookmarked}
></div>
