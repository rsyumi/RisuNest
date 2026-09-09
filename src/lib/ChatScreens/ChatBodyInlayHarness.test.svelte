<script lang="ts">
    import ChatBody from './ChatBody.svelte'
    import type { FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import type { simpleCharacterArgument } from 'src/ts/parser/parser.svelte'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import { DBState } from 'src/ts/stores.svelte'
    import { untrack } from 'svelte'
    import { getStreamingThoughtPreview } from '../../ts/parser/streamingThoughtPreview'

    import type { StreamingThoughtMode } from '../../ts/storage/database.svelte'

    interface Props {
        initialTranslated?: boolean
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
        captureContext?: FrozenChatScreenshotRenderContext
        idx?: number
        captureParserIndex?: number
        name?: string
        parserProjection?: BoundedLiveChatParserProjection
        parserAbortSignal?: AbortSignal
        reactiveAssetWidth?: boolean
        initialAssetWidth?: number
        liveCharacter?: simpleCharacterArgument | null
        initialMessage?: string
        initialThoughtPreview?: boolean
        streamingThoughtMode?: StreamingThoughtMode
        deferStreamingDisplay?: boolean
    }

    let {
        initialTranslated: translated = $bindable(false),
        onCaptureSettled,
        onCaptureError,
        captureContext,
        idx = 0,
        captureParserIndex = idx,
        name = 'Frozen Character',
        parserProjection,
        parserAbortSignal,
        reactiveAssetWidth = false,
        initialAssetWidth = -1,
        liveCharacter = null,
        initialMessage = 'first',
        initialThoughtPreview = false,
        streamingThoughtMode = 'off',
        deferStreamingDisplay = false,
    }: Props = $props()
    let message = $state(untrack(() => initialMessage))
    let previewThoughts = $state(untrack(() => initialThoughtPreview))
    export function setThoughtPreview(value: boolean) {
        previewThoughts = value
    }
    export function setThoughtMode(value: StreamingThoughtMode) {
        streamingThoughtMode = value
    }
    let reloadRevision = $state(0)
    export function reload() {
        reloadRevision += 1
    }

    export function setParserProjection(
        value: BoundedLiveChatParserProjection,
    ) {
        parserProjection = value
    }

    export function setParserAbortSignal(value: AbortSignal) {
        parserAbortSignal = value
    }

    let raw = $state(false)
    let bodyRoot = $state<HTMLElement | null>(null)
    let assetWidth = $state(untrack(() => initialAssetWidth))

    if (untrack(() => reactiveAssetWidth)) {
        Object.defineProperty(DBState.db, 'assetWidth', {
            configurable: true,
            get: () => assetWidth,
        })
    }

    export function setMessage(value: string) {
        message = value
    }

    export function setRaw(value: boolean) {
        raw = value
    }

    export function setTranslated(value: boolean) {
        translated = value
    }

    export function setAssetWidth(value: number) {
        assetWidth = value
    }
</script>

<div bind:this={bodyRoot}>
    <ChatBody
        msgDisplay={message}
        reloadRevision={String(reloadRevision)}
        {idx}
        {name}
        role="char"
        character={(captureContext?.character as simpleCharacterArgument | null) ??
            liveCharacter}
        bind:translated
        translating={false}
        retranslate={false}
        modelShortName=""
        renderRawStreaming={raw}
        rawStreamingText={previewThoughts ? message : 'streaming'}
        {streamingThoughtMode}
        {deferStreamingDisplay}
        thoughtPreview={previewThoughts
            ? getStreamingThoughtPreview(message)
            : null}
        {bodyRoot}
        {onCaptureSettled}
        {onCaptureError}
        {captureContext}
        {captureParserIndex}
        {parserProjection}
        {parserAbortSignal}
    />
</div>
