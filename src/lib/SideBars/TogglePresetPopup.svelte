<script lang="ts">
    import { DownloadIcon, PlusIcon, RefreshCwIcon, SaveIcon, UploadIcon } from '@lucide/svelte'
    import { DBState } from 'src/ts/stores.svelte'
    import { getAppliedPresetIndex } from 'src/ts/storage/presetEditor.svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertInput, alertSelect, alertToast } from 'src/ts/alert'
    import { selectSingleFile } from 'src/ts/util'
    import { downloadFile } from 'src/ts/globalApi.svelte'
    import { currentToggleKeys } from 'src/ts/toggleDefinitions'
    import {
        applyToggleValues,
        pickToggleValues,
        sanitizeToggleValues,
        snapshotToggleValues,
        type ToggleValues,
    } from 'src/ts/toggleBindings'
    import SwitchInput from '../UI/GUI/SwitchInput.svelte'
    import PresetListPopup from '../UI/PresetListPopup.svelte'

    interface Props {
        close: () => void
        values?: ToggleValues
        onApply?: (values: ToggleValues, keys: readonly string[]) => Promise<boolean>
    }
    let { close, values, onApply }: Props = $props()
    let displayedValues = $derived(values ?? DBState.db.globalChatVariables)
    type Preset = NonNullable<typeof DBState.db.togglePresets>[number]

    let showAll = $state(false)
    let promptPresetName = $derived(DBState.db.botPresets?.[getAppliedPresetIndex(DBState.db)]?.name)

    // Read back after the assignment: `??=` would hand out the raw array instead of the reactive proxy.
    const list = () => {
        DBState.db.togglePresets ??= []
        return DBState.db.togglePresets
    }
    const currentValues = () => pickToggleValues(displayedValues, currentToggleKeys())

    async function apply(preset: Preset) {
        const originalValues = displayedValues
        const mismatch = preset.promptPresetName !== promptPresetName
        const message = mismatch
            ? language.applyTogglePresetMismatchConfirm
            : language.applyTogglePresetConfirm
        if (!(await alertConfirm(message)) || displayedValues !== originalValues) return
        if (onApply) {
            if (!(await onApply(preset.values, currentToggleKeys()))) return
        } else applyToggleValues(DBState.db.globalChatVariables, preset.values, currentToggleKeys())
        alertToast(language.togglePresetApplied(preset.name))
        close()
    }
    async function saveNew() {
        const name = (await alertInput(language.togglePresetNamePrompt))?.trim()
        if (!name) return
        list().push({ name, values: currentValues(), promptPresetName })
        alertToast(language.presetSaved(name))
    }
    async function importPreset() {
        let file: Awaited<ReturnType<typeof selectSingleFile>>
        try {
            file = await selectSingleFile(['json'])
        } catch {
            return
        }
        if (!file) return
        try {
            const data = JSON.parse(new TextDecoder().decode(file.data))
            const name = typeof data?.name === 'string' ? data.name.trim() : ''
            const values = name ? sanitizeToggleValues(data.values) : null
            if (!values) {
                alertError(language.togglePresetImportError)
                return
            }
            list().push({
                name,
                values,
                promptPresetName: typeof data.promptPresetName === 'string' ? data.promptPresetName : undefined,
            })
            alertToast(language.togglePresetImported(name))
        } catch {
            alertError(language.togglePresetImportError)
        }
    }
    async function overwrite(index: number) {
        const preset = list()[index]
        if (!preset || !(await alertConfirm(language.overwriteTogglePresetConfirm(preset.name)))) return
        if (!list().includes(preset)) return
        preset.values = currentValues()
        preset.promptPresetName = promptPresetName
        alertToast(language.togglePresetOverwritten(preset.name))
    }
    async function exportPreset(preset: Preset) {
        const { name, values, promptPresetName } = $state.snapshot(preset)
        try {
            if (await downloadFile(`${name}_toggle.json`, JSON.stringify({ name, values, promptPresetName }, null, 2))) alertToast(language.togglePresetExported(name))
        } catch {
            alertError(language.togglePresetExportError)
        }
    }
    async function manageDefaults() {
        if (DBState.db.defaultToggleValues === undefined) {
            if (!(await alertConfirm(language.saveDefaultTogglesConfirm))) return
            DBState.db.defaultToggleValues = snapshotToggleValues(displayedValues)
            alertToast(language.defaultTogglesSaved)
            return
        }
        const choice = await alertSelect(
            [language.overwriteDefaultToggles, language.clearDefaultToggles, language.cancel],
            language.defaultTogglesManage,
        )
        if (choice === '0') {
            DBState.db.defaultToggleValues = snapshotToggleValues(displayedValues)
            alertToast(language.defaultTogglesSaved)
        } else if (choice === '1') {
            DBState.db.defaultToggleValues = undefined
            alertToast(language.defaultTogglesCleared)
        }
    }
</script>

<PresetListPopup
    title={language.togglePresets}
    {close}
    {list}
    presets={DBState.db.togglePresets}
    empty={language.togglePresetsEmpty}
    emptyFiltered={language.togglePresetsEmptyFiltered}
    filter={(preset) => showAll || preset.promptPresetName === promptPresetName}
    movable={showAll}
    rowTitle={language.apply}
    subtitle={(preset) => preset.promptPresetName ?? language.togglePresetNoPromptPreset}
    {apply}
    copy={(preset, name) => ({ ...$state.snapshot(preset), name })}
    confirmDelete={(preset) => alertConfirm(language.deleteTogglePresetConfirm(preset.name))}
>
    {#snippet header()}
        <SwitchInput name={language.showAllTogglePresets} bind:check={showAll} />
    {/snippet}
    {#snippet rowActions(preset, index, guarded, small)}
        <button class={small} onclick={() => guarded(() => overwrite(index))}
            ><RefreshCwIcon size={14} />{language.overwriteTogglePreset}</button
        >
        <button class={small} onclick={() => exportPreset(preset)}
            ><DownloadIcon size={14} />{language.exportTogglePreset}</button
        >
    {/snippet}
    {#snippet footer(guarded, action)}
        <div class="flex gap-2">
            <button class="{action} flex-1 min-w-0" onclick={() => guarded(saveNew)}>
                <PlusIcon size={16} class="shrink-0" /><span class="truncate">{language.saveNewPreset}</span>
            </button>
            <button class="{action} flex-1 min-w-0" onclick={importPreset}>
                <UploadIcon size={16} class="shrink-0" /><span class="truncate">{language.importTogglePreset}</span>
            </button>
        </div>
        <div class="border-t border-darkborderc pt-3 flex flex-col gap-1">
            <div class="text-xs text-textcolor2 px-0.5">{language.toggleBinding}</div>
            <button
                class="{action} w-full {DBState.db.defaultToggleValues !== undefined ? 'border-selected' : ''}"
                onclick={() => guarded(manageDefaults)}
            >
                <SaveIcon size={16} class="shrink-0" />
                <span class="truncate"
                    >{DBState.db.defaultToggleValues === undefined
                        ? language.saveDefaultToggles
                        : language.defaultTogglesManage}</span
                >
            </button>
            <SwitchInput name={language.disableToggleBinding} bind:check={DBState.db.disableToggleBinding} />
        </div>
    {/snippet}
</PresetListPopup>
