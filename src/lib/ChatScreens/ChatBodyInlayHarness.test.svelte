<script lang="ts">
    import ChatBody from './ChatBody.svelte'
    import type { FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import type { simpleCharacterArgument } from 'src/ts/parser/parser.svelte'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import { DBState } from 'src/ts/stores.svelte'
    import { untrack } from 'svelte'

    interface Props {
        initialTranslated?: boolean
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
        captureContext?: FrozenChatScreenshotRenderContext
        idx?: number
        captureParserIndex?: number
        name?: string
        parserProjection?: BoundedLiveChatParserProjection
        reactiveAssetWidth?: boolean
        initialAssetWidth?: number
        liveCharacter?: simpleCharacterArgument | null
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
        reactiveAssetWidth = false,
        initialAssetWidth = -1,
        liveCharacter = null,
    }: Props = $props()
    let message = $state('first')
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
        rawStreamingText="streaming"
        {bodyRoot}
        {onCaptureSettled}
        {onCaptureError}
        {captureContext}
        {captureParserIndex}
        {parserProjection}
    />
</div>
