<script lang="ts">
    import { onMount, untrack, type Snippet } from 'svelte'
    import { language } from 'src/lang'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { getPersistentDataRuntime } from 'src/ts/storage/persistentDataRuntime.svelte'
    import type { CompleteConversationLease } from 'src/ts/storage/activeWorkingSet.svelte'
    import { isMetadataOnlySelectedConversation } from 'src/ts/storage/selectedConversationLifecycle'
    import { isCatalogCharacterStub } from 'src/ts/storage/workingSetCatalog'
    import { isConversationSummaryStub } from 'src/ts/storage/conversationResidency'
    import LoadingIndicator from '../UI/GUI/LoadingIndicator.svelte'

    let {
        children,
        active = true,
        close,
    }: { children: Snippet; active?: boolean; close?: () => void } = $props()
    let sourceVersion = $state(0)
    let ready = $state(false)
    let failed = $state(false)
    const runtime = getPersistentDataRuntime()
    let currentLease: CompleteConversationLease | undefined

    onMount(() =>
        runtime.subscribeActiveConversationViewportSource(() => {
            // Revision notifications do not change ownership. Retain the
            // mounted inputs (and focus) while autosaving the same session.
            if (
                currentLease &&
                runtime.getActiveConversationSession() === currentLease.session
            )
                return
            sourceVersion++
        }),
    )

    // Upstream editors bind directly to DBState, including mount-time defaults.
    // Mount them only after complete ownership has been adopted by the coordinator.
    $effect(() => {
        sourceVersion
        const enabled = active
        const character = enabled
            ? DBState.db.characters[$selectedCharID]
            : null
        const conversationId = character?.chats[character.chatPage]?.id
        ready = false
        failed = false
        let disposed = false
        let lease: CompleteConversationLease | undefined
        if (character) {
            untrack(() => {
                const target = runtime.captureSelectedConversationTarget()
                if (!target) {
                    // Nonpersistent playgrounds and complete upstream working sets
                    // have no selected session. Never expose a partial shell here.
                    const conversation = character.chats[character.chatPage]
                    ready =
                        !isCatalogCharacterStub(character) &&
                        !!conversation &&
                        !isConversationSummaryStub(conversation) &&
                        !isMetadataOnlySelectedConversation(conversation)
                    failed = !ready
                    return
                }
                if (
                    target.characterId !== character.chaId ||
                    target.conversationId !== conversationId
                ) {
                    failed = true
                    return
                }
                void runtime
                    .acquireCompleteConversation('bound-editor', target)
                    .then((acquired) => {
                        if (disposed) {
                            acquired.release()
                            return
                        }
                        lease = acquired
                        currentLease = acquired
                        ready = true
                    })
                    .catch(() => {
                        if (!disposed) failed = true
                    })
            })
        }
        return () => {
            disposed = true
            if (currentLease === lease) currentLease = undefined
            lease?.release()
        }
    })
</script>

{#if active}
    {#if ready}
        {@render children()}
    {:else if failed}
        <div class="p-2 text-textcolor" role="alert">
            <span>{language.error}</span>
            <button
                class="ml-2 rounded-md border border-darkborderc px-3 py-1"
                onclick={() => sourceVersion++}
                >{language.risuNest.storage.retry}</button
            >
        </div>
    {:else}
        <div class="p-2 text-textcolor">
            <LoadingIndicator label={language.loadingChatData} compact />
        </div>
    {/if}
    {#if !ready && close}
        <button
            class="m-2 rounded-md border border-darkborderc bg-darkbg px-3 py-1 text-textcolor"
            onclick={close}>{language.cancel}</button
        >
    {/if}
{/if}
