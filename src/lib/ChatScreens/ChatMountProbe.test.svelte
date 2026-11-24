<script lang="ts">
    import { onDestroy, onMount, settled as domSettled, untrack } from 'svelte'
    import type { StreamingDisplayOptimizationMode } from 'src/ts/storage/database.svelte'
    import { chatMountProbe } from './chatMountProbe.testSupport'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import type { ChatDisplayRefresh, ChatPresentationRefresh } from 'src/ts/chatDisplayRefresh'
    import type { ChatEditorDraft } from 'src/ts/chatEditorDrafts'

    let {
        message,
        idx,
        img,
        character,
        rawStreamingText,
        bookmarked = false,
        currentPage = 1,
        totalPages = 1,
        parserProjection,
        parserAbortSignal,
        onDisplaySettled,
        restoredEditor,
        onEditorClose,
    }: {
        message: string
        idx: number
        img: string
        character: unknown
        rawStreamingText: string
        bookmarked?: boolean
        currentPage?: number
        totalPages?: number
        parserProjection?: BoundedLiveChatParserProjection
        parserAbortSignal?: AbortSignal
        onDisplaySettled?: () => void
        restoredEditor?: ChatEditorDraft
        onEditorClose?: () => void
    } = $props()

    const instanceId = chatMountProbe.nextInstanceId++
    if (untrack(() => idx) >= 0 && chatMountProbe.throwNextMount) {
        chatMountProbe.throwNextMount = false
        throw new Error('chat mount probe failure')
    }
    let displayedStreamingText = $state('')
    let refreshCount = $state(0)
    let optimizedStreaming = $state(true)

    export function hasStreamingPreview() {
        return optimizedStreaming && displayedStreamingText.includes('<Thoughts>')
    }

    export function updateStreamingDisplay(state: {
        isOptimizedStreamingMessage: boolean
        streamingOptimizationMode: StreamingDisplayOptimizationMode
        rawStreamingText: string
    }) {
        displayedStreamingText = state.rawStreamingText
        optimizedStreaming = state.isOptimizedStreamingMessage
        chatMountProbe.streamingUpdates.push({
            instanceId,
            rawStreamingText: state.rawStreamingText,
            isOptimizedStreamingMessage: state.isOptimizedStreamingMessage,
        })
    }

    let restored = $state<ChatEditorDraft | undefined>()

    export function updateViewportBinding(state: { viewportRow: { key: string; absoluteIndex: number } }) {
        idx = state.viewportRow.absoluteIndex
        chatMountProbe.viewportBindings.push({
            instanceId,
            index: idx,
            rowKey: state.viewportRow.key,
        })
    }
    export function updatePresentation(state: ChatPresentationRefresh) {
        img = state.img
        bookmarked = state.bookmarked
    }
    export function updateCandidatePosition(page: number, total: number) {
        currentPage = page
        totalPages = total
    }
    export function captureEditorDraft() {
        const open = chatMountProbe.editorDrafts.get(instanceId) ?? restored
        return open ? { ...open, index: idx, caret: undefined } : null
    }
    export function restoreEditor(draft: ChatEditorDraft) {
        restored = draft
        chatMountProbe.restored.push({ instanceId, draft })
    }
    export function hasActiveEditor() {
        return chatMountProbe.activeEditors.has(instanceId) || restored !== undefined
    }
    const initialEditor = untrack(() => restoredEditor)
    if (initialEditor) restoreEditor(initialEditor)
    function reportDisplay(settled?: () => void) {
        if (chatMountProbe.holdDisplay) chatMountProbe.pendingDisplays.set(instanceId, () => {
            chatMountProbe.pendingDisplays.delete(instanceId)
            settled?.()
        })
        else settled?.()
    }
    export function refreshMessageDisplay(state: ChatDisplayRefresh) {
        message = state.message
        if (state.index !== undefined) idx = state.index
        if (state.character !== undefined) character = state.character
        parserProjection = state.parserProjection
        parserAbortSignal = state.parserAbortSignal
        onDisplaySettled = state.onDisplaySettled
        const settled = onDisplaySettled
        void domSettled().then(() => reportDisplay(settled))
        refreshCount = untrack(() => refreshCount) + 1
        chatMountProbe.displayUpdates.push({
            instanceId,
            index: idx,
            message,
            signal: parserAbortSignal,
        })
    }

    export function refreshParserProjection(
        projection?: BoundedLiveChatParserProjection,
    ) {
        parserProjection = projection
        refreshCount = untrack(() => refreshCount) + 1
    }

    onMount(() => {
        const settled = onDisplaySettled
        void domSettled().then(() => reportDisplay(settled))
        chatMountProbe.closeEditors.set(instanceId, () => {
            chatMountProbe.activeEditors.delete(instanceId)
            restored = undefined
            onEditorClose?.()
        })
        displayedStreamingText = rawStreamingText
        chatMountProbe.mounts.push({
            instanceId,
            message,
            index: idx,
            image: img,
            character,
            bookmarked,
            parserProjectionKind: parserProjection?.kind,
            projectedChatID: parserProjection?.projectedChatID,
            parserAbortSignal,
        })
    })

    onDestroy(() => {
        chatMountProbe.pendingDisplays.delete(instanceId)
        chatMountProbe.closeEditors.delete(instanceId)
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
    data-candidate-page={currentPage}
    data-candidate-total={totalPages}
    data-refresh-count={refreshCount}
    data-restored-draft={restored?.draft}
></div>
