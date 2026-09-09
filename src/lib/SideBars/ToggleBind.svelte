<script lang="ts">
    import { PinIcon, PinOffIcon, SaveIcon, Settings2Icon } from '@lucide/svelte'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { alertError } from 'src/ts/alert'
    import { captureChatBindingTarget, saveChatBinding, updateChatBinding } from 'src/ts/chatBindings.svelte'
    import { snapshotToggleValues, countToggleChanges, applyToggleValues } from 'src/ts/toggleBindings'
    import { getModuleToggles } from 'src/ts/process/modules'
    import { parseToggleSyntax } from 'src/ts/util'
    let expanded = $state(false)
    let chat = $derived(
        DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID]?.chatPage],
    )
    let bound = $derived(chat?.savedToggleValues !== undefined)
    let changes = $derived(
        bound ? countToggleChanges(DBState.db.globalChatVariables, chat.savedToggleValues!) : 0,
    )
    async function save(unbind = false) {
        const target = captureChatBindingTarget()
        if (!target || DBState.db.disableToggleBinding) return
        updateChatBinding(target.conversation, {
            savedToggleValues: unbind ? undefined : snapshotToggleValues(DBState.db.globalChatVariables),
        })
        try {
            await saveChatBinding()
        } catch (error) {
            alertError(String(error))
        }
    }
    function applyPreset(index: number) {
        const preset = DBState.db.togglePresets?.[index]
        if (!preset) return
        const character = DBState.db.characters[$selectedCharID]
        const definitions = `${DBState.db.customPromptTemplateToggle ?? ''}\n${getModuleToggles()}\n${character?.type === 'character' ? (character.customModuleToggle ?? '') : ''}`
        applyToggleValues(
            DBState.db.globalChatVariables,
            preset.values,
            parseToggleSyntax(definitions).map((toggle) => `toggle_${toggle.key}`),
        )
    }
</script>

<div class="w-full flex flex-col gap-1 text-sm mt-2">
    <div class="flex items-center gap-1">
        <span class="text-xs text-textcolor2 flex-1">{language.toggleBinding}</span>
        <button
            disabled={DBState.db.disableToggleBinding}
            class="p-2 rounded-md hover:bg-selected disabled:opacity-40"
            title={bound ? language.unbindToggles : language.bindToggles}
            onclick={() => save(bound)}
        >
            {#if bound}<PinOffIcon size={16} />{:else}<PinIcon size={16} />{/if}
        </button>
        {#if bound && changes > 0}
            <button
                disabled={DBState.db.disableToggleBinding}
                class="flex items-center gap-1 p-2 rounded-md hover:bg-selected disabled:opacity-40"
                title={language.saveToggleChanges}
                onclick={() => save()}><SaveIcon size={16} />{changes}</button
            >
        {/if}
        <button
            class="p-2 rounded-md hover:bg-selected"
            title={language.toggleBindingOptions}
            aria-expanded={expanded}
            onclick={() => {
                expanded = !expanded
            }}><Settings2Icon size={16} /></button
        >
    </div>
    {#if bound}<span class="text-xs text-textcolor2"
            >{DBState.db.disableToggleBinding ? language.toggleBindingDisabled : language.togglesBound}</span
        >{/if}
    {#if chat?.GLGlobalVariables && Object.keys(chat.GLGlobalVariables).some( (key) => key.includes('toggle_'), )}
        <span class="text-xs text-textcolor2">{language.localTogglePriority}</span>
    {/if}
    {#if expanded}
        <label class="flex items-center gap-2 min-h-10"
            ><input
                type="checkbox"
                bind:checked={DBState.db.disableToggleBinding}
            />{language.disableToggleBinding}</label
        >
        <button
            class="p-2 rounded-md hover:bg-selected text-left"
            onclick={() => {
                DBState.db.defaultToggleValues =
                    DBState.db.defaultToggleValues === undefined
                        ? snapshotToggleValues(DBState.db.globalChatVariables)
                        : undefined
            }}
            >{DBState.db.defaultToggleValues === undefined
                ? language.saveDefaultToggles
                : language.clearDefaultToggles}</button
        >
        {#if DBState.db.togglePresets?.length}
            <select
                class="w-full p-2 bg-darkbutton border border-darkborderc rounded-md"
                aria-label={language.togglePresets}
                value=""
                onchange={(event) => {
                    applyPreset(Number(event.currentTarget.value))
                    event.currentTarget.value = ''
                }}
            >
                <option value="" disabled>{language.togglePresets}</option>
                {#each DBState.db.togglePresets as preset, index}<option value={index}>{preset.name}</option
                    >{/each}
            </select>
        {/if}
    {/if}
</div>
