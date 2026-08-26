<script lang="ts">
    import type { character } from 'src/ts/storage/database.svelte'
    import { language } from 'src/lang'
    import Chat from './Chat.svelte'
    import CreatorQuote from './CreatorQuote.svelte'

    let {
        currentCharacter,
        resolvedImage,
        showAiWarning,
        onReroll,
        unReroll,
        onRemoveCreatorQuote,
    }: {
        currentCharacter: character
        resolvedImage: string
        showAiWarning: boolean
        onReroll: () => void
        unReroll: () => void
        onRemoveCreatorQuote: () => void
    } = $props()

    let currentChat = $derived(currentCharacter.chats[currentCharacter.chatPage])
    let alternateGreetings = $derived(currentCharacter.alternateGreetings ?? [])
    let greeting = $derived(
        currentChat.fmIndex === -1
            ? currentCharacter.firstMessage
            : alternateGreetings[currentChat.fmIndex] ?? currentCharacter.firstMessage,
    )
</script>

<div data-chat-conversation-start-content>
    <Chat
        character={{
            type: 'simple',
            chaId: currentCharacter.chaId,
            virtualscript: currentCharacter.virtualscript,
            customscript: currentCharacter.customscript,
            additionalAssets: currentCharacter.additionalAssets,
            emotionImages: currentCharacter.emotionImages,
            triggerscript: currentCharacter.triggerscript,
        }}
        name={currentCharacter.name}
        message={greeting}
        role="char"
        img={resolvedImage}
        idx={-1}
        altGreeting={alternateGreetings.length > 0}
        largePortrait={currentCharacter.largePortrait}
        firstMessage={true}
        {onReroll}
        {unReroll}
        isLastMemory={false}
        currentPage={(currentChat.fmIndex ?? -1) + 2}
        totalPages={alternateGreetings.length + 1}
    />
    {#if showAiWarning && currentChat.message.length === 0}
        <div class="ml-auto mr-auto mt-4 text-textcolor2 italic max-w-2/3 wrap-break-word text-center">
            {language.aiGenerationWarning}
        </div>
    {/if}
    {#if !currentCharacter.removedQuotes && (currentCharacter.creatorNotes?.length ?? 0) >= 2}
        <CreatorQuote quote={currentCharacter.creatorNotes} onRemove={onRemoveCreatorQuote} />
    {/if}
</div>
