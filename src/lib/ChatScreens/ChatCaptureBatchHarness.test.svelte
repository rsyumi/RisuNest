<script lang="ts">
    import Chat from './Chat.svelte'
    import type { FrozenChatScreenshotRenderContext } from 'src/ts/chatScreenshotRange'
    import type { Message } from 'src/ts/storage/database.svelte'

    let {
        messages,
        captureContext,
        firstIndex = 0,
    }: {
        messages: Message[]
        captureContext: FrozenChatScreenshotRenderContext
        firstIndex?: number
    } = $props()
</script>

{#each messages as message, index}
    <Chat
        message={message.data}
        name={message.role === 'user' ? captureContext.userName : captureContext.characterName}
        isLastMemory={false}
        idx={firstIndex + index}
        role={message.role}
        totalLength={firstIndex + messages.length}
        character={captureContext.character as import('src/ts/parser/parser.svelte').simpleCharacterArgument | null}
        captureMessage={message}
        {captureContext}
        captureParserIndex={index}
    />
{/each}
