<script module lang="ts">
    export type PresetManagerMode = 'select' | 'bind'

    export interface PresetManagerProps {
        close: () => void
        /** `bind` makes a row click bind that preset to the chat instead of selecting it. */
        mode?: PresetManagerMode
        /** Preset ID the chat is bound to, empty or absent when it uses the current preset. Bind mode only. */
        boundId?: string
        /** Preset ID the current chat uses, marked on its row when that row is not highlighted. */
        chatPresetId?: string
        /** Binds the preset with this ID, or unbinds with null. Resolving to false keeps the list open. */
        onBind?: (id: string | null) => boolean | void | Promise<boolean | void>
    }
</script>

<script lang="ts">
    import CheckIcon from '@lucide/svelte/icons/check'
    import GitCompareIcon from '@lucide/svelte/icons/git-compare'
    import HardDriveUploadIcon from '@lucide/svelte/icons/hard-drive-upload'
    import PlusIcon from '@lucide/svelte/icons/plus'
    import SearchIcon from '@lucide/svelte/icons/search'
    import Share2Icon from '@lucide/svelte/icons/share-2'
    import { language } from 'src/lang'
    import { alertCardExport, alertConfirm, alertError, alertInput, alertToast } from 'src/ts/alert'
    import { DBState, ShowRealmFrameStore } from 'src/ts/stores.svelte'
    import {
        addPreset,
        changeToPreset,
        copyPreset,
        downloadPreset,
        importPreset,
        movePreset,
        readPresetBodies,
        removePreset,
        renamePreset,
        type botPreset,
    } from 'src/ts/storage/database.svelte'
    import { assertPresetNames, capturePresetNames } from 'src/ts/storage/presetWorkingSetOperations'
    import { prebuiltPresets } from 'src/ts/process/templates/templates'
    import { RISU_PRESET_DRAG_TYPE } from 'src/ts/dragTypes'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import PresetListPopup, { type PresetGuard, type PresetListActions } from '../UI/PresetListPopup.svelte'
    import PromptDiffModal from '../Others/PromptDiffModal.svelte'
    import { reportPresetOperation } from './presetOperation'

    let { close, mode = 'select', boundId, chatPresetId, onBind }: PresetManagerProps = $props()

    type NamedPreset = botPreset & { name: string }
    const text = language.risuNest.presetList

    let query = $state('')
    let needle = $derived(query.trim().toLocaleLowerCase())
    let comparing = $state(false)
    /** The first preset picked for a comparison, kept by identity so a reorder does not change it. */
    let comparePick = $state.raw<botPreset | null>(null)
    let diff = $state.raw<[botPreset, botPreset] | null>(null)
    let selectionName = $derived(DBState.db.botPresets[DBState.db.botPresetsId]?.name ?? '')
    let inherits = $derived(!boundId)

    const snapshot = () => capturePresetNames(DBState.db.botPresets)
    const run = (operation: () => Promise<unknown>) =>
        reportPresetOperation(operation, () => alertError(language.presetActionFailed))
    const highlighted = (preset: NamedPreset, index: number) =>
        mode === 'bind' ? !!boundId && preset.id === boundId : index === DBState.db.botPresetsId

    const actions: PresetListActions = {
        async rename(index) {
            const expected = snapshot()
            const previous = DBState.db.botPresets[index]?.name ?? ''
            const name = (await alertInput(language.renamePreset, [], previous))?.trim()
            if (!name || name === previous) return false
            if (!(await run(() => renamePreset(index, name, expected)))) return false
            alertToast(language.presetRenamed(previous, name))
            return true
        },
        async duplicate(index) {
            let added = -1
            if (!(await run(async () => {
                added = await copyPreset(index, snapshot())
            }))) return false
            const name = DBState.db.botPresets[added]?.name
            if (name !== undefined) alertToast(language.presetDuplicated(name))
            return true
        },
        async remove(index) {
            if (DBState.db.botPresets.length === 1) {
                alertError(language.errors.onlyOneChat)
                return false
            }
            const expected = snapshot()
            const name = DBState.db.botPresets[index]?.name ?? ''
            if (!(await alertConfirm(`${language.removeConfirm}${name}`))) return false
            if (!(await run(() => removePreset(index, expected)))) return false
            alertToast(language.presetDeleted(name))
            return true
        },
        move: (from, to) => run(() => movePreset(from, to, snapshot())),
    }

    async function exportPreset(index: number) {
        const expected = snapshot()
        const data = await alertCardExport('preset')
        if (data.type === '') {
            await run(() => downloadPreset(index, 'risupreset', expected))
        }
        if (data.type === 'realm') {
            await run(async () => {
                assertPresetNames(DBState.db.botPresets, expected)
                ShowRealmFrameStore.set(`preset:${index}`)
            })
        }
    }

    async function bind(id: string | null) {
        let result: boolean | void
        try {
            result = await onBind?.(id)
        } catch (error) {
            alertError(error instanceof Error ? error : String(error))
            return
        }
        if (result !== false) close()
    }

    async function compareWith(preset: botPreset) {
        if (comparePick === preset) {
            comparePick = null
            return
        }
        if (!comparePick) {
            comparePick = preset
            return
        }
        const first = DBState.db.botPresets.indexOf(comparePick)
        const second = DBState.db.botPresets.indexOf(preset)
        comparePick = null
        comparing = false
        if (first < 0 || second < 0) {
            alertError(language.presetActionFailed)
            return
        }
        try {
            const [firstBody, secondBody] = await readPresetBodies([first, second])
            diff = [firstBody, secondBody]
        } catch (error) {
            alertError(error instanceof Error ? error.message : String(error))
        }
    }

    async function apply(preset: NamedPreset, index: number) {
        if (comparing) return compareWith(preset)
        if (mode === 'bind') {
            if (preset.id) await bind(preset.id)
            return
        }
        if (await run(() => changeToPreset(index))) close()
    }

    async function addNew() {
        const preset = structuredClone(prebuiltPresets.OAI2)
        preset.name = 'New Preset'
        await run(() => addPreset(preset))
    }

    async function importFromFile() {
        try {
            await importPreset()
        } catch (error) {
            alertError(error instanceof Error ? error : String(error))
        }
    }

    function toggleCompare() {
        comparing = !comparing
        comparePick = null
    }
</script>

{#snippet inheritRow(guarded: PresetGuard)}
    <button
        class="w-full border border-darkborderc rounded-md px-3 py-2 pointer-coarse:py-2.5 text-left transition-colors hover:bg-selected flex items-center gap-2"
        aria-current={inherits ? 'true' : undefined}
        onclick={() =>
            guarded(async () => {
                if (!comparing) await bind(null)
            })}
    >
        <span class="min-w-0 flex-1 truncate {inherits ? 'font-medium' : ''}">{text.inherit} ({selectionName})</span>
        {#if inherits}<CheckIcon size={16} class="shrink-0" />{/if}
    </button>
{/snippet}

<PresetListPopup
    title={language.presets}
    {close}
    list={() => DBState.db.botPresets as NamedPreset[]}
    presets={DBState.db.botPresets as NamedPreset[]}
    emptyFiltered={text.noMatch}
    filter={(preset) => !needle || (preset.name ?? '').toLocaleLowerCase().includes(needle)}
    movable={!needle}
    selected={highlighted}
    {apply}
    {actions}
    dragType={RISU_PRESET_DRAG_TYPE}
    layer="absolute inset-0 z-40"
    width="w-[28rem]"
    listScroll
    touchTargets
    leading={mode === 'bind' ? inheritRow : undefined}
>
    {#snippet header()}
        <label class="relative block">
            <span class="sr-only">{language.search}</span>
            <SearchIcon
                size={16}
                class="absolute left-2.5 top-1/2 -translate-y-1/2 text-textcolor2 pointer-events-none"
            />
            <input
                class="border border-darkborderc focus:border-borderc rounded-md shadow-xs text-textcolor bg-transparent focus:ring-borderc focus:ring-2 focus:outline-hidden transition-colors duration-200 w-full pl-8 pr-2 py-1.5 pointer-coarse:py-2 text-sm"
                placeholder={language.search}
                bind:value={query}
            />
        </label>
    {/snippet}
    {#snippet rowLeading(preset: NamedPreset, index: number)}
        <!-- The Ctrl+number shortcuts change the selection, so binding shows no numbers. -->
        {#if mode !== 'bind'}
            {#if index < 9}
                <span class="shrink-0 w-5 text-right text-xs text-subtext tabular-nums">{index + 1}</span>
            {:else}
                <span class="shrink-0 w-5" aria-hidden="true"></span>
            {/if}
        {/if}
        {#if preset.image}
            <img
                src={preset.image}
                alt=""
                class="size-6 shrink-0 rounded-sm object-cover"
                decoding="async"
                draggable="false"
            />
        {/if}
    {/snippet}
    {#snippet rowTrailing(preset: NamedPreset, index: number)}
        {#if comparePick === preset}
            <span class="shrink-0 text-green-500" title={text.compare}><GitCompareIcon size={16} /></span>
        {/if}
        {#if chatPresetId && preset.id === chatPresetId && !highlighted(preset, index)}
            <span class="shrink-0 rounded-sm border border-darkborderc px-1 text-xs text-subtext">{text.chatPreset}</span>
        {/if}
    {/snippet}
    {#snippet rowActionsEnd(_preset: NamedPreset, index: number, guarded: PresetGuard, small: string)}
        <button class={small} onclick={() => guarded(() => exportPreset(index))}
            ><Share2Icon size={14} />{text.export}</button
        >
    {/snippet}
    {#snippet footer(guarded: PresetGuard, action: string)}
        <div class="flex flex-wrap items-center gap-2">
            <button class={action} onclick={() => guarded(addNew)}
                ><PlusIcon size={16} class="shrink-0" />{language.add}</button
            >
            <button class={action} onclick={() => guarded(importFromFile)}
                ><HardDriveUploadIcon size={16} class="shrink-0" />{text.import}</button
            >
            {#if DBState.db.showPromptComparison}
                <button
                    class="{action} aria-pressed:border-selected aria-pressed:bg-selected"
                    aria-pressed={comparing}
                    onclick={toggleCompare}><GitCompareIcon size={16} class="shrink-0" />{text.compare}</button
                >
            {/if}
        </div>
        {#if comparing || mode === 'select'}<p class="text-xs text-subtext m-0" aria-live="polite">{comparing ? text.compareHint : language.quickPreset}</p>{/if}
    {/snippet}
</PresetListPopup>

{#if diff}
    <div class="contents" use:modalNavigation={{ close: () => (diff = null) }}>
        <PromptDiffModal firstPreset={diff[0]} secondPreset={diff[1]} onClose={() => (diff = null)} />
    </div>
{/if}
