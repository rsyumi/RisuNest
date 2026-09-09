<script lang="ts">
    import type { character, Message } from 'src/ts/storage/database.svelte'
    import Chats from './Chats.svelte'
    import type { ChatViewportHandle, ChatViewportJumpOptions } from 'src/ts/chatViewport'
    import type { ConversationViewportSource } from 'src/ts/conversationViewportSource'
    import type {
        LiveChatParserConversationStartRequest,
        LiveChatParserProjectionResolver,
    } from 'src/ts/selectedConversationLiveParserProjection'

    let {
        initialMessages,
        initialCharacter,
        initialViewportSource = null,
        initialViewportNavigationGeneration = 0,
        parserProjectionResolver,
        acquireConversationStartParserLease,
    }: {
        initialMessages?: Message[]
        initialCharacter: character
        initialViewportSource?: ConversationViewportSource | null
        initialViewportNavigationGeneration?: number
        parserProjectionResolver?: LiveChatParserProjectionResolver
        acquireConversationStartParserLease?: (
            request: LiveChatParserConversationStartRequest,
        ) => Promise<{ release(): void } | null>
    } = $props()

    let messages = $state<Message[] | undefined>()
    let currentCharacter = $state<character>(null as unknown as character)
    let chats = $state<ChatViewportHandle>()
    let viewportSource = $state<ConversationViewportSource | null>(null)
    let viewportNavigationGeneration = $state(0)
    let hasNewUnreadMessage = $state(false)
    const initialize = () => {
        messages = initialMessages
        currentCharacter = initialCharacter
        viewportSource = initialViewportSource
        viewportNavigationGeneration = initialViewportNavigationGeneration
    }
    initialize()

    export function setMessages(nextMessages: Message[]) {
        messages = nextMessages
        currentCharacter.chats[currentCharacter.chatPage].message = nextMessages
    }

    export function updateMessage(index: number, data: string) {
        if (messages) messages[index].data = data
    }

    export function setStreaming(isStreaming: boolean) {
        currentCharacter.chats[currentCharacter.chatPage].isStreaming = isStreaming
    }

    export function replaceParserDependencies() {
        currentCharacter.customscript = [...currentCharacter.customscript]
    }

    export function mutateAssetTuple(path: string) {
        currentCharacter.additionalAssets[0][1] = path
    }

    export function mutateScriptOutput(output: string) {
        currentCharacter.customscript[0].out = output
    }

    export function setImage(image: string) {
        currentCharacter.image = image
    }

    export function setViewportSource(nextSource: ConversationViewportSource | null) {
        viewportSource = nextSource
    }

    export function setViewportNavigationGeneration(generation: number) {
        viewportNavigationGeneration = generation
    }

    export function switchCharacter(character: character, nextMessages: Message[]) {
        currentCharacter = character
        messages = nextMessages
    }

    export function switchCharacterAndSource(
        character: character,
        nextSource: ConversationViewportSource,
    ) {
        currentCharacter = character
        messages = undefined
        viewportSource = nextSource
    }

    export function jumpTo(index: number, options?: ChatViewportJumpOptions) {
        return chats?.jumpTo(index, options) ?? Promise.resolve(false)
    }

    export function jumpToLatestMessage() {
        return chats?.jumpToLatestMessage() ?? Promise.resolve()
    }

    export function hasUnreadMessage() {
        return hasNewUnreadMessage
    }

    export function getCurrentCharacter() {
        return currentCharacter
    }
</script>

<div class="scroll-parent">
    <Chats
        bind:this={chats}
        {messages}
        {viewportSource}
        {viewportNavigationGeneration}
        {parserProjectionResolver}
        {acquireConversationStartParserLease}
        {currentCharacter}
        onReroll={() => {}}
        unReroll={() => {}}
        currentUsername="User"
        userIcon="user.png"
        bind:hasNewUnreadMessage
    />
</div>
