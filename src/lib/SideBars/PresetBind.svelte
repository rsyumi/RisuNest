<script lang="ts">
    import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
    import { DBState, selectedCharID } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { bindPresetFromList, captureChatBindingTarget, chatBindingBlockedByGeneration } from 'src/ts/chatBindings.svelte'
    import { getAppliedPresetId } from 'src/ts/storage/presetEditor.svelte'
    import LazyScreenError from '../UI/LazyScreenError.svelte'
    import LoadingIndicator from '../UI/GUI/LoadingIndicator.svelte'

    let target = $state<ReturnType<typeof captureChatBindingTarget>>(null)
    let retry = $state(0)
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
    const loadList = (_retry: number) => import('../Setting/PresetManager.svelte')
</script>

{#if chat}
    <div class="flex flex-col gap-1 w-full">
        <div class="text-xs text-textcolor2 px-0.5">{language.risuNest.presetBinding.label}</div>
        <button
            class="flex items-center gap-2 w-full min-h-10 px-3 py-2 rounded-md bg-darkbutton border text-left transition-colors hover:bg-selected {bound
                ? 'border-selected text-textcolor'
                : 'border-darkborderc text-textcolor2 hover:text-textcolor'}"
            title={language.risuNest.presetBinding.label}
            onclick={() => {
                if (chatBindingBlockedByGeneration()) return
                target = captureChatBindingTarget()
            }}
        >
            <SlidersHorizontalIcon size={16} class="shrink-0" />
            <span class="min-w-0 truncate text-sm">{label}</span>
        </button>
    </div>
{/if}
{#if target}
    {@const bindTarget = target}
    <!-- The list sits on the modal layer so it also opens above the editor popups that show this row. -->
    <div class="fixed inset-0 z-modal">
        {#await loadList(retry)}
            <div class="w-full h-full flex items-center justify-center bg-black/50 text-textcolor">
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const PresetManager = module.default}
            <PresetManager
                mode="bind"
                boundId={bindTarget.conversation.promptPresetId}
                chatPresetId={getAppliedPresetId()}
                onBind={(id) => bindPresetFromList(bindTarget, id)}
                close={() => {
                    target = null
                }}
            />
        {:catch}
            <LazyScreenError
                overlay
                message={language.risuNest.lazy.screen}
                onRetry={() => { retry++ }}
                backLabel={language.close}
                onBack={() => { target = null }}
            />
        {/await}
    </div>
{/if}
