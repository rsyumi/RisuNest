<script lang="ts">
    import { CheckIcon, SearchIcon, SettingsIcon, Waypoints, XIcon } from "@lucide/svelte";
    import { language } from "src/lang";
    import { DBState, selectedCharID, SettingsMenuIndex, settingsOpen } from "src/ts/stores.svelte";
    import { modalNavigation } from "src/ts/ui/modalNavigation";
    import { isConversationSummaryStub } from "src/ts/storage/conversationResidency";
    import { setModuleScope } from "src/ts/moduleScopes";
    import { moduleScopeState, type ModuleScope } from "src/ts/process/moduleSelection";

    let { close }: { close: () => void } = $props();

    const text = $derived(language.risuNest.moduleScope);
    const titleId = `module-scope-title-${Math.random().toString(36).slice(2)}`;
    let search = $state('');
    // Scope edits still being stored, shown as already made.
    let pending = $state<Record<string, boolean>>({});

    const character = $derived(DBState.db.characters[$selectedCharID]);
    const chat = $derived(character?.chats?.[character.chatPage]);
    const chatAvailable = $derived(!!chat && !isConversationSummaryStub(chat));

    const rows = $derived.by(() => {
        const query = search.trim().toLocaleLowerCase();
        const sources = {
            enabledModules: DBState.db.enabledModules,
            moduleIntergration: DBState.db.moduleIntergration,
            characterModules: character?.modules,
            chatModules: chatAvailable ? chat?.modules : undefined,
        };
        return DBState.db.modules
            .filter((rmodule) => !query || rmodule.name.toLocaleLowerCase().includes(query))
            .sort((a, b) => a.name.toLocaleLowerCase().localeCompare(b.name.toLocaleLowerCase()))
            .map((rmodule) => ({ rmodule, scopeState: moduleScopeState(rmodule, sources) }));
    });

    const scopes = $derived<{ scope: ModuleScope; label: string; available: boolean }[]>([
        { scope: 'global', label: text.global, available: true },
        { scope: 'character', label: text.character, available: !!character },
        { scope: 'chat', label: text.chat, available: chatAvailable },
    ]);

    async function toggle(scope: ModuleScope, moduleId: string, on: boolean) {
        const key = `${scope}:${moduleId}`;
        if (key in pending) return;
        pending[key] = !on;
        try {
            await setModuleScope(scope, moduleId, !on);
        } finally {
            delete pending[key];
        }
    }

    function openModuleSettings() {
        $SettingsMenuIndex = 14;
        $settingsOpen = true;
        close();
    }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div
    class="absolute inset-0 z-40 flex items-center justify-center bg-black/50 sm:p-6"
    onclick={(event) => { if (event.target === event.currentTarget) close() }}
>
    <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        class="module-scope-menu flex h-full w-full flex-col bg-darkbg text-textcolor sm:h-auto sm:max-h-full sm:max-w-2xl sm:rounded-lg sm:border sm:border-darkborderc sm:shadow-2xl"
        use:modalNavigation={{ close }}
    >
        <header class="flex items-start gap-3 px-4 pt-4 pb-3 sm:px-5">
            <div class="min-w-0 grow">
                <h2 id={titleId} class="m-0 text-lg font-semibold">{language.modules}</h2>
                <p class="mt-1 mb-0 text-sm break-keep text-textcolor2">{text.info}</p>
            </div>
            <button
                type="button"
                class="-mt-1 -mr-1 shrink-0 rounded-md p-1.5 text-textcolor2 transition-colors hover:bg-selected hover:text-textcolor focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-borderc"
                aria-label={language.close}
                title={language.close}
                onclick={close}
            >
                <XIcon size={20} />
            </button>
        </header>

        {#if DBState.db.modules.length > 0}
            <div class="px-4 pb-3 sm:px-5">
                <label class="flex items-center gap-2 rounded-md border border-darkborderc px-3 transition-colors focus-within:border-borderc focus-within:ring-2 focus-within:ring-borderc">
                    <SearchIcon size={16} class="shrink-0 text-textcolor2" />
                    <input
                        type="search"
                        class="min-w-0 grow bg-transparent py-2 text-sm text-textcolor outline-hidden placeholder:text-textcolor2"
                        placeholder={language.search}
                        aria-label={language.search}
                        autocomplete="off"
                        bind:value={search}
                    />
                </label>
            </div>
        {/if}

        <div class="min-h-0 flex-1 overflow-y-auto border-t border-darkborderc sm:min-h-24">
            {#if DBState.db.modules.length === 0}
                <p class="m-0 px-4 py-8 text-center text-sm text-textcolor2 sm:px-5">{language.noModules}</p>
            {:else if rows.length === 0}
                <p class="m-0 px-4 py-8 text-center text-sm text-textcolor2 sm:px-5">{text.noMatch}</p>
            {:else}
                <ul class="m-0 list-none divide-y divide-darkborderc/60 p-0">
                    {#each rows as { rmodule, scopeState } (rmodule.id)}
                        <li class="flex flex-col gap-2 px-4 py-3 sm:flex-row sm:items-center sm:gap-4 sm:px-5" data-module-scope-row={rmodule.id}>
                            <div class="flex min-w-0 grow items-center gap-2">
                                {#if rmodule.mcp}
                                    <Waypoints size={16} class="shrink-0 text-textcolor2" />
                                {/if}
                                <span class="min-w-0 truncate text-sm {scopeState.applied ? 'font-medium text-textcolor' : 'text-textcolor2'}" title={rmodule.name}>{rmodule.name}</span>
                                {#if scopeState.integrated}
                                    <span class="shrink-0 rounded-sm border border-darkborderc px-1.5 py-px text-xs text-textcolor2">{language.moduleIntergration}</span>
                                {/if}
                            </div>
                            <div role="group" aria-label={rmodule.name} class="grid shrink-0 grid-cols-3 gap-1.5 sm:flex">
                                {#each scopes as { scope, label, available } (scope)}
                                    {@const on = pending[`${scope}:${rmodule.id}`] ?? scopeState[scope]}
                                    <button
                                        type="button"
                                        class="scope-chip inline-flex min-h-10 min-w-0 items-center justify-center gap-1.5 rounded-md border px-2.5 text-sm transition-colors focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-borderc disabled:cursor-not-allowed disabled:opacity-40 sm:min-h-8 sm:min-w-[5.5rem] {on
                                            ? 'border-borderc bg-selected text-textcolor'
                                            : 'border-darkborderc text-textcolor2 enabled:hover:bg-selected/40 enabled:hover:text-textcolor'}"
                                        aria-pressed={on}
                                        disabled={!available}
                                        data-module-scope={scope}
                                        onclick={() => void toggle(scope, rmodule.id, on)}
                                    >
                                        <span class="flex size-4 shrink-0 items-center justify-center rounded-sm {on ? 'bg-textcolor text-darkbg' : 'border border-current opacity-70'}" aria-hidden="true">
                                            {#if on}<CheckIcon size={12} strokeWidth={3.5} />{/if}
                                        </span>
                                        <span class="truncate">{label}</span>
                                    </button>
                                {/each}
                            </div>
                        </li>
                    {/each}
                </ul>
            {/if}
        </div>

        <footer class="flex justify-end border-t border-darkborderc px-4 py-3 sm:px-5">
            <button
                type="button"
                class="inline-flex min-h-9 items-center gap-1.5 rounded-md border border-darkborderc bg-darkbutton px-3 text-sm text-textcolor transition-colors hover:bg-selected focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-borderc"
                onclick={openModuleSettings}
            >
                <SettingsIcon size={16} class="shrink-0" />
                <span>{language.edit}</span>
            </button>
        </footer>
    </div>
</div>
