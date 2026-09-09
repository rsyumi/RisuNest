<script lang="ts">
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import { ContactIcon } from '@lucide/svelte'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { bindPersona, captureChatBindingTarget, saveChatBinding } from 'src/ts/chatBindings.svelte'
    import { alertError } from 'src/ts/alert'
    import ListedPersona from '../Setting/listedPersona.svelte'
    let target = $state<ReturnType<typeof captureChatBindingTarget>>(null)
    let chat = $derived(
        DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID]?.chatPage],
    )
    let bound = $derived(
        DBState.db.personas.find((persona) => persona.id && persona.id === chat?.bindedPersona),
    )
    async function select(index: number) {
        if (!target?.isCurrent()) return
        try {
            await bindPersona(target.conversation, index)
            await saveChatBinding()
        } catch (error) {
            alertError(String(error))
        }
    }
</script>

<button
    class="flex items-center gap-2 w-full min-h-10 px-3 py-2 rounded-md bg-darkbutton border border-darkborderc text-left"
    onclick={() => {
        target = captureChatBindingTarget()
    }}
>
    <ContactIcon size={18} class="shrink-0" />
    <span class="min-w-0 truncate text-sm"
        >{bound?.name ??
            (chat?.bindedPersona
                ? language.missingBoundPersona
                : `${language.inheritPersona} (${DBState.db.username})`)}</span
    >
</button>
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
