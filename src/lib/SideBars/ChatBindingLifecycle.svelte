<script lang="ts">
    import { untrack } from 'svelte'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { applyConversationPresetBinding, flushEffectiveToggleEdits, deriveEffectiveToggleVariables } from 'src/ts/storage/database.svelte'
    import { isLoadedPreset } from 'src/ts/storage/effectiveIdentityState'
    import { doingChat } from 'src/ts/process/generationState'
    import { isConversationSummaryStub } from 'src/ts/storage/conversationResidency'
    import { activeRerollConversations, recoverInterruptedReroll } from 'src/ts/durableReroll'
    import { getHistoryWindowMemoryMode, openSelectedHistoryWindow, type SelectedHistoryWindow } from 'src/ts/process/index.svelte'
    import {
        acquireCompleteConversation,
        captureSelectedConversationTarget,
        flushPendingData,
    } from 'src/ts/storage/persistentDataRuntime.svelte'
    import { alertError } from 'src/ts/alert'
    import { refreshAuthorNoteCopy } from 'src/ts/chatBindings.svelte'
    let recovering = new Set<string>()
    $effect(() => {
        const character = DBState.db?.characters?.[$selectedCharID]
        const chat = character?.chats?.[character.chatPage]
        const active = $activeRerollConversations
        if (!chat?.rerollRecovery || active.includes(chat.id) || recovering.has(chat.id)) return
        const target = captureSelectedConversationTarget()
        if (!target) return
        recovering.add(chat.id)
        const startIndex = chat.rerollRecovery.startIndex
        const isSelected = () => {
            const selected = DBState.db.characters[$selectedCharID]
            const current = selected?.chats[selected.chatPage]
            return selected?.chaId === character.chaId && current?.id === chat.id ? current : null
        }
        void (async () => {
            let lease: Awaited<ReturnType<typeof acquireCompleteConversation>> | undefined
            let window: SelectedHistoryWindow | null = null
            try {
                if (getHistoryWindowMemoryMode(true) !== null) {
                    // The tail from the message before the reroll holds everything recovery rewrites.
                    window = await openSelectedHistoryWindow({ tailStart: () => Math.max(startIndex - 1, 0) })
                    if (!window || window.chat.id !== chat.id || !isSelected()) return
                    recoverInterruptedReroll(window.chat, window.controller)
                } else {
                    lease = await acquireCompleteConversation('recover-reroll', target)
                    const current = isSelected()
                    if (!current) return
                    recoverInterruptedReroll(current, lease.session)
                }
                await flushPendingData('recover-reroll')
            } catch (error) {
                alertError(error)
            } finally {
                window?.release()
                lease?.release()
                recovering.delete(chat.id)
            }
        })()
    })
    $effect(() => {
        const db = DBState.db
        const character = db?.characters?.[$selectedCharID]
        const chat = character?.chats?.[character.chatPage]
        void db?.disableToggleBinding
        void chat?.savedToggleValues
        if (!db) return
        untrack(() => {
            flushEffectiveToggleEdits(db)
            deriveEffectiveToggleVariables(db, chat && !isConversationSummaryStub(chat) ? chat : undefined)
        })
    })
    // A running request keeps the preset of the conversation it generates for, so the selected
    // conversation's binding applies once it ends.
    $effect(() => {
        const db = DBState.db
        const character = db?.characters?.[$selectedCharID]
        const chat = character?.chats?.[character.chatPage]
        const id = chat?.promptPresetId
        // Follows the bound preset being added, removed or replaced by its stored body.
        if (id) void isLoadedPreset(db.botPresets?.find((preset) => preset?.id === id))
        if (!db || $doingChat) return
        untrack(() => applyConversationPresetBinding(db, chat)).catch((error) => console.error(error))
    })
    // Runs when another conversation is selected or finishes loading, not on every preset edit.
    $effect(() => {
        const character = DBState.db?.characters?.[$selectedCharID]
        const chat = character?.chats?.[character.chatPage]
        if (!chat || untrack(() => isConversationSummaryStub(chat))) return
        // A failed copy is retried on the next selection; readers resolve the preset meanwhile.
        untrack(() => refreshAuthorNoteCopy()).catch(() => {})
    })
</script>
