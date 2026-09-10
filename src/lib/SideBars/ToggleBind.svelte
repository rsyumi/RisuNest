<script lang="ts">
    import { FolderHeartIcon, PinIcon, SaveIcon } from '@lucide/svelte'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertToast } from 'src/ts/alert'
    import { captureChatBindingTarget, saveChatBinding, updateChatBinding } from 'src/ts/chatBindings.svelte'
    import { countToggleChanges, snapshotToggleValues, type ToggleValues } from 'src/ts/toggleBindings'
    import TogglePresetPopup from './TogglePresetPopup.svelte'
    let presetsOpen = $state(false)
    let chat = $derived(
        DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID]?.chatPage],
    )
    let disabled = $derived(!!DBState.db.disableToggleBinding)
    let bound = $derived(chat?.savedToggleValues !== undefined)
    let changes = $derived(
        bound && !disabled ? countToggleChanges(DBState.db.globalChatVariables, chat.savedToggleValues!) : 0,
    )
    let hasLocalOverrides = $derived(
        Object.keys(chat?.GLGlobalVariables ?? {}).some((key) => key.startsWith('toggle_')),
    )
    async function write(values: ToggleValues | undefined, message: string) {
        const target = captureChatBindingTarget()
        if (!target || disabled) return
        try {
            await updateChatBinding(target.conversation, { savedToggleValues: values })
            await saveChatBinding()
            alertToast(message)
        } catch (error) {
            alertError(String(error))
        }
    }
    const bind = () => write(snapshotToggleValues(DBState.db.globalChatVariables), language.togglesBound)
    async function unbind() {
        if (!(await alertConfirm(language.unbindTogglesConfirm))) return
        await write(undefined, language.togglesUnbound)
    }
    const button =
        'inline-flex items-center justify-center gap-1.5 min-h-10 px-3 rounded-md border text-sm transition-colors disabled:opacity-40 disabled:pointer-events-none'
    const neutral = 'bg-darkbutton border-darkborderc text-textcolor hover:bg-selected'
</script>

<div class="flex flex-col gap-1 w-full">
    <div class="text-xs text-textcolor2 px-0.5">{language.toggleBinding}</div>
    <div class="flex gap-1 items-stretch">
        {#if bound}
            <button
                class="{button} shrink-0 w-10 px-0 bg-primary-500 border-primary-500 text-white hover:bg-primary-600"
                title={language.unbindToggles}
                aria-pressed="true"
                {disabled}
                onclick={unbind}><PinIcon size={16} /></button
            >
            <button
                class="{button} flex-1 min-w-0 {changes > 0
                    ? 'bg-draculared/15 border-draculared/40 text-draculared hover:bg-draculared/25'
                    : neutral}"
                title={language.saveToggleChanges}
                disabled={disabled || changes === 0}
                onclick={() => bind()}
            >
                <SaveIcon size={16} class="shrink-0" />
                <span class="truncate">{changes > 0 ? changes : language.saveTogglesLabel}</span>
            </button>
        {:else}
            <button
                class="{button} {neutral} flex-1 min-w-0"
                title={language.bindToggles}
                aria-pressed="false"
                {disabled}
                onclick={() => bind()}
            >
                <PinIcon size={16} class="shrink-0" />
                <span class="truncate">{language.bindTogglesLabel}</span>
            </button>
        {/if}
        <button
            class="{button} {neutral} shrink-0 w-10 px-0"
            title={language.togglePresets}
            aria-haspopup="dialog"
            onclick={() => {
                presetsOpen = true
            }}><FolderHeartIcon size={16} /></button
        >
    </div>
    {#if disabled}
        <span class="text-xs text-textcolor2 px-0.5">{language.toggleBindingDisabled}</span>
    {/if}
    {#if hasLocalOverrides}
        <span class="text-xs text-textcolor2 px-0.5">{language.localTogglePriority}</span>
    {/if}
</div>
{#if presetsOpen}
    <TogglePresetPopup
        close={() => {
            presetsOpen = false
        }}
    />
{/if}
