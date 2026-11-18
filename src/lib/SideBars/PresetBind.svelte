<script lang="ts">
    import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
    import { DBState, openPresetList, selectedCharID } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { captureChatBindingTarget, chatBindingBlockedByGeneration, presetListBinding } from 'src/ts/chatBindings.svelte'

    let chat = $derived(
        DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID]?.chatPage],
    )
    let bound = $derived(
        DBState.db.botPresets?.find((preset) => preset?.id && preset.id === chat?.promptPresetId),
    )
    let selectionName = $derived(DBState.db.botPresets?.[DBState.db.botPresetsId]?.name ?? '')
    let label = $derived(
        bound?.name ??
            (chat?.promptPresetId
                ? language.risuNest.presetBinding.missing
                : `${language.risuNest.presetList.inherit} (${selectionName})`),
    )
</script>

<div class="flex flex-col gap-1 w-full">
    <div class="text-xs text-textcolor2 px-0.5">{language.risuNest.presetBinding.label}</div>
    <button
        class="flex items-center gap-2 w-full min-h-10 px-3 py-2 rounded-md bg-darkbutton border text-left transition-colors hover:bg-selected {bound
            ? 'border-selected text-textcolor'
            : 'border-darkborderc text-textcolor2 hover:text-textcolor'}"
        title={language.risuNest.presetBinding.label}
        onclick={() => {
            if (chatBindingBlockedByGeneration()) return
            const target = captureChatBindingTarget()
            if (!target) return
            presetListBinding.target = target
            $openPresetList = true
        }}
    >
        <SlidersHorizontalIcon size={16} class="shrink-0" />
        <span class="min-w-0 truncate text-sm">{label}</span>
    </button>
</div>
