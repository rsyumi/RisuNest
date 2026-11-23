<script lang="ts">
    import { FolderHeartIcon, PinIcon, SaveIcon } from '@lucide/svelte'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertToast } from 'src/ts/alert'
    import { captureChatBindingTarget, chatBindingBlockedByGeneration, saveChatBinding, updateChatBinding } from 'src/ts/chatBindings.svelte'
    import { countToggleChanges, snapshotToggleValues, toggleValueChanged, type ToggleValues } from 'src/ts/toggleBindings'
    import TogglePresetPopup from './TogglePresetPopup.svelte'
    interface Props {
        values?: ToggleValues
        unpinKeys?: string[]
        onApplyPreset?: (values: ToggleValues, keys: readonly string[]) => Promise<boolean>
        saving?: boolean
        /** Leaves out unbinding and the toggle presets, so a bound chat offers only saving its changed values. */
        saveOnly?: boolean
    }
    let { values, unpinKeys = [], onApplyPreset, saving = $bindable(false), saveOnly = false }: Props = $props()
    let currentValues = $derived(values ?? DBState.db.globalChatVariables)
    let presetsOpen = $state(false)
    let chat = $derived(
        DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID]?.chatPage],
    )
    let disabled = $derived(!!DBState.db.disableToggleBinding)
    let bound = $derived(chat?.savedToggleValues !== undefined)
    let changes = $derived(
        bound && !disabled ? countToggleChanges(currentValues, chat.savedToggleValues!)
            + unpinKeys.filter(key => !toggleValueChanged(currentValues[key], chat.savedToggleValues![key])).length : 0,
    )
    let hasLocalOverrides = $derived(
        Object.keys(chat?.GLGlobalVariables ?? {}).some((key) => key.startsWith('toggle_')),
    )
    async function write(values: ToggleValues | undefined, message: string, target = captureChatBindingTarget()) {
        if (chatBindingBlockedByGeneration()) return
        if (!target?.isCurrent() || disabled || saving) return
        const patch = { savedToggleValues: values }
        const localValues = values !== undefined && unpinKeys.length > 0
            ? { ...target.conversation.GLGlobalVariables } : undefined
        if (localValues) for (const key of unpinKeys) delete localValues[key]
        saving = true
        try {
            await updateChatBinding(target.conversation, localValues ? { ...patch, GLGlobalVariables: localValues } : patch)
            await saveChatBinding()
            alertToast(message)
        } catch (error) {
            alertError(language.toggleBindingSaveFailed)
        } finally {
            saving = false
        }
    }
    const bind = () => write(snapshotToggleValues(currentValues), language.togglesBound)
    async function unbind() {
        if (chatBindingBlockedByGeneration()) return
        const target = captureChatBindingTarget()
        if (!(await alertConfirm(language.unbindTogglesConfirm)) || !target?.isCurrent()) return
        await write(undefined, language.togglesUnbound, target)
    }
    const button =
        'inline-flex items-center justify-center gap-1.5 min-h-10 px-3 rounded-md border text-sm transition-colors disabled:opacity-40 disabled:pointer-events-none'
    const neutral = 'bg-darkbutton border-darkborderc text-textcolor hover:bg-selected'
</script>

<div class="flex flex-col gap-1 w-full">
    <div class="text-xs text-textcolor2 px-0.5">{language.toggleBinding}</div>
    <div class="flex gap-1 items-stretch">
        {#if bound}
            {#if !saveOnly}
                <button
                    class="{button} shrink-0 w-10 px-0 bg-primary-500 border-primary-500 text-primary-foreground hover:bg-primary-600"
                    title={language.unbindToggles}
                    aria-pressed="true"
                    disabled={disabled || saving}
                    onclick={unbind}><PinIcon size={16} /></button
                >
            {/if}
            <button
                class="{button} flex-1 min-w-0 {changes > 0
                    ? 'bg-draculared/15 border-draculared/40 text-draculared hover:bg-draculared/25'
                    : neutral}"
                title={language.saveToggleChanges}
                disabled={disabled || saving || changes === 0}
                onclick={() => bind()}
            >
                <SaveIcon size={16} class="shrink-0" />
                <span class="truncate"
                    >{changes > 0
                        ? `${language.saveTogglesLabel} (${changes})`
                        : language.saveTogglesLabel}</span
                >
            </button>
        {:else}
            <button
                class="{button} {neutral} flex-1 min-w-0"
                title={language.bindToggles}
                aria-pressed="false"
                disabled={disabled || saving}
                onclick={() => bind()}
            >
                <PinIcon size={16} class="shrink-0" />
                <span class="truncate">{language.bindTogglesLabel}</span>
            </button>
        {/if}
        {#if !saveOnly}
            <button
                class="{button} {neutral} shrink-0 w-10 px-0"
                title={language.togglePresets}
                aria-haspopup="dialog"
                disabled={saving}
                onclick={() => {
                    presetsOpen = true
                }}><FolderHeartIcon size={16} /></button
            >
        {/if}
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
        values={currentValues}
        onApply={onApplyPreset}
        close={() => {
            presetsOpen = false
        }}
    />
{/if}
