<script lang="ts">
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import { ContactIcon } from '@lucide/svelte'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { bindPersona, captureChatBindingTarget, chatBindingBlockedByGeneration, saveChatBinding } from 'src/ts/chatBindings.svelte'
    import { alertError } from 'src/ts/alert'
    import ListedPersona from '../Setting/listedPersona.svelte'
    let target = $state<ReturnType<typeof captureChatBindingTarget>>(null)
    let chat = $derived(
        DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID]?.chatPage],
    )
    let bound = $derived(
        DBState.db.personas.find((persona) => persona.id && persona.id === chat?.bindedPersona),
    )
    // `username` is what chats actually use; the persona entry only seeds it.
    let currentPersona = $derived(
        DBState.db.username || DBState.db.personas[DBState.db.selectedPersona]?.name || '',
    )
    let label = $derived(
        bound?.name ??
            (chat?.bindedPersona
                ? language.missingBoundPersona
                : `${language.inheritPersona} (${currentPersona})`),
    )
    // Like `username`, `userNote` holds the current persona's live note.
    let note = $derived(bound ? bound.note : chat?.bindedPersona ? '' : DBState.db.userNote)
    async function select(index: number) {
        if (!target?.isCurrent() || chatBindingBlockedByGeneration()) return
        try {
            await bindPersona(target.conversation, index)
            await saveChatBinding()
        } catch (error) {
            alertError(language.personaBindingSaveFailed)
        }
    }
</script>

<div class="flex flex-col gap-1 w-full">
    <div class="text-xs text-textcolor2 px-0.5">{language.personaBinding}</div>
    <button
        class="flex items-center gap-2 w-full min-h-10 px-3 py-2 rounded-md bg-darkbutton border text-left transition-colors hover:bg-selected {bound
            ? 'border-selected text-textcolor'
            : 'border-darkborderc text-textcolor2 hover:text-textcolor'}"
        title={language.personaBinding}
        onclick={() => {
            if (chatBindingBlockedByGeneration()) return
            target = captureChatBindingTarget()
        }}
    >
        <ContactIcon size={16} class="shrink-0" />
        <span class="min-w-0 truncate text-sm">{label}</span>
        {#if note}
            <span class="min-w-0 flex-1 truncate text-xs opacity-75">({note})</span>
        {/if}
    </button>
</div>
{#if target}
    <div
        class="fixed inset-0 z-modal"
        use:modalNavigation={{
            close: () => {
                target = null
            },
        }}
    >
        <ListedPersona
            bindingMode
            selectedId={target.conversation.bindedPersona}
            onSelect={select}
            close={() => {
                target = null
            }}
        />
    </div>
{/if}
