<script lang="ts">
    import { ChevronRightIcon, PackageIcon } from "@lucide/svelte";
    import { language } from "src/lang";
    import { DBState, moduleScopeMenuOpen, selectedCharID } from "src/ts/stores.svelte";
    import { isConversationSummaryStub } from "src/ts/storage/conversationResidency";
    import { moduleScopeState } from "src/ts/process/moduleSelection";

    const appliedCount = $derived.by(() => {
        const character = DBState.db.characters[$selectedCharID];
        const chat = character?.chats?.[character.chatPage];
        const sources = {
            enabledModules: DBState.db.enabledModules,
            moduleIntergration: DBState.db.moduleIntergration,
            characterModules: character?.modules,
            chatModules: chat && !isConversationSummaryStub(chat) ? chat.modules : undefined,
        };
        return DBState.db.modules.filter((rmodule) => moduleScopeState(rmodule, sources).applied).length;
    });
</script>

<button
    type="button"
    class="mt-3 flex min-h-10 w-full items-center gap-2 rounded-md border border-darkborderc bg-darkbutton px-3 py-2 text-left text-sm text-textcolor transition-colors hover:bg-selected focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-borderc"
    data-module-scope-open
    onclick={() => moduleScopeMenuOpen.set(true)}
>
    <PackageIcon size={16} class="shrink-0" />
    <span class="min-w-0 grow truncate">{language.modules}</span>
    {#if appliedCount > 0}
        <span class="shrink-0 rounded-full bg-selected px-2 py-px text-xs tabular-nums text-textcolor">{appliedCount}</span>
    {/if}
    <ChevronRightIcon size={16} class="shrink-0 text-textcolor2" />
</button>
