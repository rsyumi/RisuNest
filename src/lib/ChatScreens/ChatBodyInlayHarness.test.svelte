<script lang="ts">
    import ChatBody from './ChatBody.svelte'

    interface Props {
        initialTranslated?: boolean
        onCaptureSettled?: (generation: number) => void
    }

    let {
        initialTranslated: translated = $bindable(false),
        onCaptureSettled,
    }: Props = $props()
    let message = $state('first')
    let raw = $state(false)

    export function setMessage(value: string) {
        message = value
    }

    export function setRaw(value: boolean) {
        raw = value
    }
</script>

<ChatBody
    msgDisplay={message}
    role="char"
    bind:translated
    translating={false}
    retranslate={false}
    modelShortName=""
    renderRawStreaming={raw}
    rawStreamingText="streaming"
    {onCaptureSettled}
/>
