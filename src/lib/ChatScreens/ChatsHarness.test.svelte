<script lang="ts">
    import type { character, Message } from 'src/ts/storage/database.svelte'
    import Chats from './Chats.svelte'
    import type { ChatViewportHandle, ChatViewportJumpOptions } from 'src/ts/chatViewport'

    let {
        initialMessages,
        initialCharacter,
    }: {
        initialMessages: Message[]
        initialCharacter: character
    } = $props()

    let messages = $state<Message[]>([])
    let currentCharacter = $state<character>(null as unknown as character)
    let chats = $state<ChatViewportHandle>()
    const initialize = () => {
        messages = initialMessages
        currentCharacter = initialCharacter
    }
    initialize()

    export function setMessages(nextMessages: Message[]) {
        messages = nextMessages
        currentCharacter.chats[currentCharacter.chatPage].message = nextMessages
    }

    export function updateMessage(index: number, data: string) {
        messages[index].data = data
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

    export function switchCharacter(character: character, nextMessages: Message[]) {
        currentCharacter = character
        messages = nextMessages
    }

    export function jumpTo(index: number, options?: ChatViewportJumpOptions) {
        return chats?.jumpTo(index, options) ?? Promise.resolve(false)
    }

    export function jumpToLatestMessage() {
        return chats?.jumpToLatestMessage() ?? Promise.resolve()
    }
</script>

<div class="scroll-parent">
    <Chats
        bind:this={chats}
        {messages}
        {currentCharacter}
        onReroll={() => {}}
        unReroll={() => {}}
        currentUsername="User"
        userIcon="user.png"
    />
</div>
