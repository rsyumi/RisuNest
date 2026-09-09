<script lang="ts">
    import { onMount, untrack, type Snippet } from 'svelte'
    import {
        acquireLiveDisplayParserLease,
        captureLiveDisplayParserInputs,
        captureLiveDisplayParserSelection,
        subscribeLiveDisplayParserSelection,
    } from 'src/ts/liveDisplayParserLease'
    import { getCurrentCharacter, getCurrentChat } from 'src/ts/storage/database.svelte'
    import { language } from 'src/lang'
    import LoadingIndicator from '../UI/GUI/LoadingIndicator.svelte'

    let {
        source,
        character,
        children,
    }: {
        source: unknown
        character: Parameters<typeof captureLiveDisplayParserInputs>[1]
        children: Snippet<[AbortSignal]>
    } = $props()
    let readySignal = $state<AbortSignal | null>(null)
    let failed = $state(false)
    let retry = $state(0)
    let activeController: AbortController | null = null
    let selectionIdentity = $state(untrack(captureLiveDisplayParserSelection))
    onMount(() =>
        subscribeLiveDisplayParserSelection((identity) => {
            if (identity !== selectionIdentity) activeController?.abort()
            selectionIdentity = identity
        }),
    )
    const signature = $derived(
        JSON.stringify({
            characterId: getCurrentCharacter()?.chaId,
            conversationId: getCurrentChat()?.id,
            inputs: captureLiveDisplayParserInputs(source, character),
        }),
    )

    $effect(() => {
        void signature
        void selectionIdentity
        void retry
        const controller = new AbortController()
        activeController = controller
        let lease: { release(): void } | null = null
        readySignal = null
        failed = false
        void untrack(() =>
            acquireLiveDisplayParserLease({ source, character, signal: controller.signal }),
        )
            .then((acquired) => {
                if (controller.signal.aborted) {
                    acquired?.release()
                    return
                }
                lease = acquired
                readySignal = controller.signal
            })
            .catch(() => {
                if (!controller.signal.aborted) failed = true
            })
        return () => {
            controller.abort()
            if (activeController === controller) activeController = null
            lease?.release()
        }
    })
</script>

{#if readySignal}
    {@render children(readySignal)}
{:else if failed}
    <div role="alert" data-live-display-load-error>
        {language.chatDataLoadFailed}
        <button onclick={() => (retry += 1)}>{language.hypaV3Modal.retry}</button>
    </div>
{:else}
    <LoadingIndicator label={language.loadingChatData} />
{/if}
