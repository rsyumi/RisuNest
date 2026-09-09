<script lang="ts">
    import { onMount, untrack, type Snippet } from 'svelte'
    import {
        acquireLiveDisplayParserLease,
        captureLiveDisplayParserInputs,
        captureLiveDisplayParserSelection,
        subscribeLiveDisplayParserSelection,
    } from 'src/ts/liveDisplayParserLease'
    import {
        getCurrentCharacter,
        getCurrentChat,
    } from 'src/ts/storage/database.svelte'
    import { language } from 'src/lang'
    import LoadingIndicator from '../UI/GUI/LoadingIndicator.svelte'

    let {
        source,
        character,
        children,
        refreshRevision = 0,
        preservePendingContent = false,
    }: {
        source: unknown
        character: Parameters<typeof captureLiveDisplayParserInputs>[1]
        children: Snippet<[AbortSignal]>
        refreshRevision?: number
        preservePendingContent?: boolean
    } = $props()
    let readySignal = $state<AbortSignal | null>(null)
    let failed = $state(false)
    let retry = $state(0)
    let activeController: AbortController | null = null
    let readyOwner: string | null = null
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
    const owner = $derived(
        JSON.stringify([
            getCurrentCharacter()?.chaId,
            getCurrentChat()?.id,
            selectionIdentity,
        ]),
    )

    $effect(() => {
        void signature
        void selectionIdentity
        void retry
        void refreshRevision
        const requestedOwner = owner
        const controller = new AbortController()
        activeController = controller
        let lease: { release(): void } | null = null
        // Retain the published DOM while a refresh is admitted, but never carry
        // another conversation's markup or styles across navigation.
        if (!preservePendingContent || readyOwner !== requestedOwner)
            readySignal = null
        failed = false
        void untrack(() =>
            acquireLiveDisplayParserLease({
                source,
                character,
                signal: controller.signal,
            }),
        )
            .then((acquired) => {
                if (controller.signal.aborted) {
                    acquired?.release()
                    return
                }
                lease = acquired
                readyOwner = requestedOwner
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
{/if}
{#if failed}
    <div role="alert" data-live-display-load-error>
        {language.chatDataLoadFailed}
        <button onclick={() => (retry += 1)}>{language.hypaV3Modal.retry}</button>
    </div>
{:else if !readySignal}
    <LoadingIndicator label={language.loadingChatData} />
{/if}
