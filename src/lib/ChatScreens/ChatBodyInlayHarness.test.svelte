<script lang="ts">
    import ChatBody from './ChatBody.svelte'
    import type { FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import type { simpleCharacterArgument } from 'src/ts/parser/parser.svelte'

    interface Props {
        initialTranslated?: boolean
        onCaptureSettled?: (generation: number) => void
        onCaptureError?: (generation: number, error: unknown) => void
        captureContext?: FrozenChatScreenshotRenderContext
        idx?: number
        captureParserIndex?: number
        name?: string
    }

    let {
        initialTranslated: translated = $bindable(false),
        onCaptureSettled,
        onCaptureError,
        captureContext,
        idx = 0,
        captureParserIndex = idx,
        name = 'Frozen Character',
    }: Props = $props()
    let message = $state('first')
    let raw = $state(false)
    let bodyRoot = $state<HTMLElement | null>(null)

    export function setMessage(value: string) {
        message = value
    }

    export function setRaw(value: boolean) {
        raw = value
    }
</script>

<div bind:this={bodyRoot}>
    <ChatBody
        msgDisplay={message}
        {idx}
        {name}
        role="char"
        character={captureContext?.character as simpleCharacterArgument | null}
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
    />
</div>
