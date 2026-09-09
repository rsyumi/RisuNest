<script lang="ts">
    import type { character } from 'src/ts/storage/database.svelte'
    import { language } from 'src/lang'
    import Chat from './Chat.svelte'
    import CreatorQuote from './CreatorQuote.svelte'
    import { onMount } from 'svelte'
    import LoadingIndicator from 'src/lib/UI/GUI/LoadingIndicator.svelte'
    import type { LiveChatParserConversationStartRequest } from 'src/ts/selectedConversationLiveParserProjection'
    import type { SelectedConversationOperations } from 'src/ts/selectedConversationOperations'

    let {
        currentCharacter,
        resolvedImage,
        showAiWarning,
        totalMessages,
        onReroll,
        unReroll,
        onRemoveCreatorQuote,
        acquireConversationStartParserLease,
        selectedConversationOperations,
    }: {
        currentCharacter: character
        resolvedImage: string
        showAiWarning: boolean
        totalMessages: number
        onReroll: () => void
        unReroll: () => void
        onRemoveCreatorQuote: () => void
        acquireConversationStartParserLease?: (
            request: LiveChatParserConversationStartRequest,
        ) => Promise<{ release(): void } | null>
        selectedConversationOperations?: SelectedConversationOperations
    } = $props()

    let currentChat = $derived(
        currentCharacter.chats[currentCharacter.chatPage],
    )
    let alternateGreetings = $derived(currentCharacter.alternateGreetings ?? [])
    let greeting = $derived(
        currentChat.fmIndex === -1
            ? currentCharacter.firstMessage
            : (alternateGreetings[currentChat.fmIndex] ??
                  currentCharacter.firstMessage),
    )
    let parserReady = $state(false)
    let parserLoadFailed = $state(false)
    let parserLease: { release(): void } | null = null
    let parserController = $state<AbortController | null>(null)
    let preparingController: AbortController | null = null
    let parserRequestGeneration = 0
    let destroyed = false

    export function updateConversationStartPresentation(state: {
        resolvedImage: string
    }): void {
        resolvedImage = state.resolvedImage
    }

    async function prepareParser(): Promise<void> {
        preparingController?.abort()
        parserController?.abort()
        parserLease?.release()
        parserLease = null
        const generation = ++parserRequestGeneration
        const controller = new AbortController()
        preparingController = controller
        parserLoadFailed = false
        const isCurrent = () => !destroyed && generation === parserRequestGeneration
        try {
            const lease =
                (await acquireConversationStartParserLease?.({
                    greeting,
                    totalMessages,
                    signal: controller.signal,
                    isCurrent,
                })) ?? null
            if (!isCurrent()) {
                lease?.release()
                return
            }
            parserLease = lease
            parserController = controller
            parserReady = true
        } catch {
            if (isCurrent()) parserLoadFailed = true
        }
    }

    export function refreshConversationStartParser(): void {
        void prepareParser()
    }

    onMount(() => {
        void prepareParser()
        return () => {
            destroyed = true
            preparingController?.abort()
            parserRequestGeneration += 1
            parserController?.abort()
            parserLease?.release()
            parserLease = null
        }
    })
</script>

<div data-chat-conversation-start-content>
    {#if !currentCharacter.removedQuotes && (currentCharacter.creatorNotes?.length ?? 0) >= 2}
        <CreatorQuote
            quote={currentCharacter.creatorNotes}
            onRemove={onRemoveCreatorQuote}
        />
    {/if}
    {#if parserLoadFailed}
        <div
            class="flex items-center justify-center gap-3 p-3"
            role="alert"
            data-chat-greeting-load-error
        >
            <span>{language.chatDataLoadFailed}</span>
            <button
                class="rounded-lg border border-borderc px-3 py-1.5 hover:bg-darkbg"
                onclick={prepareParser}
            >
                {language.hypaV3Modal.retry}
            </button>
        </div>
    {:else if !parserReady}
        <LoadingIndicator label={language.loadingChatData} />
    {:else}
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
            {selectedConversationOperations}
            parserAbortSignal={parserController?.signal}
        />
    {/if}
    {#if showAiWarning && totalMessages === 0}
        <div
            class="ml-auto mr-auto mt-4 text-textcolor2 italic max-w-2/3 wrap-break-word text-center"
        >
            {language.aiGenerationWarning}
        </div>
    {/if}
</div>
