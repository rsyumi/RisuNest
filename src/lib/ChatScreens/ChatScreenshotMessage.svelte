<script lang="ts">
    import type { Message } from 'src/ts/storage/database.svelte'
    import type { DeepReadonly, FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import type { simpleCharacterArgument } from 'src/ts/parser/parser.svelte'
    import ChatBody from './ChatBody.svelte'

    interface Props {
        item: {
            turn: number
            message: DeepReadonly<Message>
        }
        context: FrozenChatScreenshotRenderContext
        portraitStyle: string
        onCaptureSettled: (generation: number) => void
        onCaptureError: (generation: number, error: unknown) => void
    }

    let { item, context, portraitStyle, onCaptureSettled, onCaptureError }: Props = $props()
    let bodyRoot = $state<HTMLElement | null>(null)
    let translated = $state(false)
    let translating = $state(false)
    let retranslate = $state(false)
    let messageName = $derived(item.message.name || (
        item.message.role === 'user' ? context.userName : context.characterName
    ))
    let portraitHeight = $derived(
        context.settings.iconSize * 3.5 / 100 / (
            item.message.role === 'user'
                ? context.userLargePortrait ? 0.75 : 1
                : context.characterLargePortrait ? 0.75 : 1
        ),
    )
</script>

<article
    class="risu-chat flex max-w-full px-6 py-3"
    class:opacity-60={item.message.disabled === true}
    data-chat-index={item.turn - 1}
    data-chat-id={item.message.chatId ?? ''}
>
    <div
        class="mr-4 shrink-0 rounded-md bg-textcolor2"
        style={`${portraitStyle}height:${portraitHeight}rem;width:${context.settings.iconSize * 3.5 / 100}rem;min-width:${context.settings.iconSize * 3.5 / 100}rem;`}
    ></div>
    <section class="min-w-0 grow">
        <div class="mb-1 font-semibold" data-character-name>{messageName}</div>
        <div
            class="chat-width chattext prose min-w-0"
            bind:this={bodyRoot}
            style:font-size={`${0.875 * (context.settings.zoomSize / 100)}rem`}
            style:line-height={`${context.settings.lineHeight * (context.settings.zoomSize / 100)}rem`}
        >
            <ChatBody
                character={context.character as simpleCharacterArgument | null}
                firstMessage={item.turn === 1}
                idx={item.turn - 1}
                msgDisplay={item.message.data}
                name={messageName}
                role={item.message.role}
                bind:translated
                bind:translating
                bind:retranslate
                {bodyRoot}
                modelShortName=""
                captureContext={context}
                {onCaptureSettled}
                {onCaptureError}
            />
        </div>
        {#if item.message.time !== undefined}
            <time class="mt-1 block text-xs text-textcolor2" data-message-time>{item.message.time}</time>
        {/if}
    </section>
</article>
