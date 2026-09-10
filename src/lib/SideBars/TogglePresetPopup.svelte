<script lang="ts">
    import {
        ChevronDownIcon,
        ChevronUpIcon,
        CopyIcon,
        DownloadIcon,
        EllipsisVerticalIcon,
        PencilIcon,
        PlusIcon,
        RefreshCwIcon,
        SaveIcon,
        TrashIcon,
        UploadIcon,
        XIcon,
    } from '@lucide/svelte'
    import { DBState } from 'src/ts/stores.svelte'
    import { language } from 'src/lang'
    import { alertConfirm, alertError, alertInput, alertSelect, alertToast } from 'src/ts/alert'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import { selectSingleFile } from 'src/ts/util'
    import { downloadFile } from 'src/ts/globalApi.svelte'
    import { currentToggleKeys } from 'src/ts/toggleDefinitions'
    import {
        applyToggleValues,
        pickToggleValues,
        sanitizeToggleValues,
        snapshotToggleValues,
    } from 'src/ts/toggleBindings'
    import SwitchInput from '../UI/GUI/SwitchInput.svelte'

    interface Props {
        close: () => void
    }
    let { close }: Props = $props()
    type Preset = NonNullable<typeof DBState.db.togglePresets>[number]

    const titleId = $props.id()
    let showAll = $state(false)
    let openMenu = $state<number | null>(null)
    /** Nested alerts in progress. Escape and Back belong to them, not to this popup. */
    let busy = $state(0)
    let promptPresetName = $derived(DBState.db.botPresets?.[DBState.db.botPresetsId]?.name)
    let presets = $derived(DBState.db.togglePresets ?? [])
    let visible = $derived(
        presets
            .map((preset, index) => ({ preset, index }))
            .filter(({ preset }) => showAll || preset.promptPresetName === promptPresetName),
    )

    // Read back after the assignment: `??=` would hand out the raw array instead of the reactive proxy.
    const list = () => {
        DBState.db.togglePresets ??= []
        return DBState.db.togglePresets
    }
    const currentValues = () => pickToggleValues(DBState.db.globalChatVariables, currentToggleKeys())
    async function guarded(work: () => Promise<void>) {
        busy++
        try {
            await work()
        } finally {
            busy--
        }
    }
    const dismiss = () => {
        if (!busy) close()
    }

    const apply = (preset: Preset) =>
        guarded(async () => {
            const mismatch = preset.promptPresetName !== promptPresetName
            const message = mismatch
                ? language.applyTogglePresetMismatchConfirm
                : language.applyTogglePresetConfirm
            if (!(await alertConfirm(message))) return
            applyToggleValues(DBState.db.globalChatVariables, preset.values, currentToggleKeys())
            alertToast(language.togglePresetApplied(preset.name))
            close()
        })
    const saveNew = () =>
        guarded(async () => {
            const name = (await alertInput(language.togglePresetNamePrompt))?.trim()
            if (!name) return
            list().push({ name, values: currentValues(), promptPresetName })
            alertToast(language.togglePresetSaved(name))
        })
    // The native file picker never resolves when it is cancelled, so this stays outside `guarded`.
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
    const overwrite = (index: number) =>
        guarded(async () => {
            const preset = list()[index]
            if (!preset || !(await alertConfirm(language.overwriteTogglePresetConfirm(preset.name)))) return
            preset.values = currentValues()
            preset.promptPresetName = promptPresetName
            alertToast(language.togglePresetOverwritten(preset.name))
        })
    const rename = (index: number) =>
        guarded(async () => {
            const preset = list()[index]
            if (!preset) return
            const name = (await alertInput(language.renameTogglePreset, [], preset.name))?.trim()
            if (!name || name === preset.name) return
            const previous = preset.name
            preset.name = name
            alertToast(language.togglePresetRenamed(previous, name))
        })
    function duplicate(index: number) {
        const presets = list()
        const preset = presets[index]
        if (!preset) return
        const copy = { ...$state.snapshot(preset), name: `${preset.name} (Copy)` }
        presets.splice(index + 1, 0, copy)
        openMenu = null
        alertToast(language.togglePresetDuplicated(copy.name))
    }
    function exportPreset(preset: Preset) {
        const { name, values, promptPresetName } = $state.snapshot(preset)
        void downloadFile(`${name}_toggle.json`, JSON.stringify({ name, values, promptPresetName }, null, 2))
        alertToast(language.togglePresetExported(name))
    }
    const remove = (index: number) =>
        guarded(async () => {
            const presets = list()
            const preset = presets[index]
            if (!preset || !(await alertConfirm(language.deleteTogglePresetConfirm(preset.name)))) return
            presets.splice(index, 1)
            openMenu = null
            alertToast(language.togglePresetDeleted(preset.name))
        })
    function move(index: number, delta: -1 | 1) {
        const presets = list()
        const target = index + delta
        if (target < 0 || target >= presets.length) return
        ;[presets[index], presets[target]] = [presets[target], presets[index]]
        openMenu = target
    }
    const manageDefaults = () =>
        guarded(async () => {
            if (DBState.db.defaultToggleValues === undefined) {
                if (!(await alertConfirm(language.saveDefaultTogglesConfirm))) return
                DBState.db.defaultToggleValues = snapshotToggleValues(DBState.db.globalChatVariables)
                alertToast(language.defaultTogglesSaved)
                return
            }
            const choice = await alertSelect(
                [language.overwriteDefaultToggles, language.clearDefaultToggles, language.cancel],
                language.defaultTogglesManage,
            )
            if (choice === '0') {
                DBState.db.defaultToggleValues = snapshotToggleValues(DBState.db.globalChatVariables)
                alertToast(language.defaultTogglesSaved)
            } else if (choice === '1') {
                DBState.db.defaultToggleValues = undefined
                alertToast(language.defaultTogglesCleared)
            }
        })

    const action =
        'inline-flex items-center justify-center gap-1.5 min-h-10 px-3 rounded-md border border-darkborderc bg-darkbutton text-sm text-textcolor transition-colors hover:bg-selected disabled:opacity-40 disabled:pointer-events-none'
    const small =
        'inline-flex items-center gap-1 h-8 px-2 rounded-md border border-darkborderc bg-darkbutton text-xs text-textcolor transition-colors hover:bg-selected disabled:opacity-40 disabled:pointer-events-none'
</script>

<div class="fixed inset-0 z-modal bg-black/50 flex justify-center items-center p-4" use:modalNavigation={{ close: dismiss }}>
    <div
        class="relative z-10 bg-darkbg text-textcolor rounded-md flex flex-col w-96 max-w-full max-h-full"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
    >
        <div class="flex items-center gap-2 p-4 pb-2">
            <h2 id={titleId} class="font-bold grow m-0">{language.togglePresets}</h2>
            <button class="text-textcolor2 hover:text-green-500 transition-colors" title={language.cancel} onclick={close}>
                <XIcon size={24} />
            </button>
        </div>
        <div class="flex flex-col gap-3 px-4 pb-4 overflow-y-auto">
            <SwitchInput name={language.showAllTogglePresets} bind:check={showAll} />
            {#if presets.length === 0}
                <p class="text-textcolor2 text-sm m-0">{language.togglePresetsEmpty}</p>
            {:else if visible.length === 0}
                <p class="text-textcolor2 text-sm m-0">{language.togglePresetsEmptyFiltered}</p>
            {:else}
                <div class="flex flex-col gap-1" role="list">
                    {#each visible as { preset, index } (index)}
                        <div
                            class="border rounded-md transition-colors {openMenu === index
                                ? 'border-selected'
                                : 'border-darkborderc'}"
                            role="listitem"
                        >
                            <div class="flex items-stretch">
                                <button
                                    class="flex-1 min-w-0 px-3 py-2 text-left rounded-l-md transition-colors hover:bg-selected"
                                    title={language.apply}
                                    onclick={() => apply(preset)}
                                >
                                    <div class="text-xs text-textcolor2 truncate">
                                        {preset.promptPresetName ?? language.togglePresetNoPromptPreset}
                                    </div>
                                    <div class="truncate">{preset.name}</div>
                                </button>
                                <button
                                    class="shrink-0 w-9 flex items-center justify-center rounded-r-md transition-colors hover:bg-selected"
                                    title={language.togglePresetActions}
                                    aria-expanded={openMenu === index}
                                    onclick={() => {
                                        openMenu = openMenu === index ? null : index
                                    }}><EllipsisVerticalIcon size={16} /></button
                                >
                            </div>
                            {#if openMenu === index}
                                <div class="flex flex-wrap gap-1 p-2 border-t border-darkborderc">
                                    {#if showAll}
                                        <button
                                            class={small}
                                            title={language.moveTogglePresetUp}
                                            disabled={index === 0}
                                            onclick={() => move(index, -1)}><ChevronUpIcon size={14} /></button
                                        >
                                        <button
                                            class={small}
                                            title={language.moveTogglePresetDown}
                                            disabled={index === presets.length - 1}
                                            onclick={() => move(index, 1)}><ChevronDownIcon size={14} /></button
                                        >
                                    {/if}
                                    <button class={small} onclick={() => overwrite(index)}
                                        ><RefreshCwIcon size={14} />{language.overwriteTogglePreset}</button
                                    >
                                    <button class={small} onclick={() => rename(index)}
                                        ><PencilIcon size={14} />{language.renameTogglePreset}</button
                                    >
                                    <button class={small} onclick={() => duplicate(index)}
                                        ><CopyIcon size={14} />{language.duplicateTogglePreset}</button
                                    >
                                    <button class={small} onclick={() => exportPreset(preset)}
                                        ><DownloadIcon size={14} />{language.exportTogglePreset}</button
                                    >
                                    <button
                                        class="{small} text-draculared border-draculared/40 hover:bg-draculared/15"
                                        onclick={() => remove(index)}
                                        ><TrashIcon size={14} />{language.deleteTogglePreset}</button
                                    >
                                </div>
                            {/if}
                        </div>
                    {/each}
                </div>
            {/if}
            <div class="flex gap-2">
                <button class="{action} flex-1 min-w-0" onclick={saveNew}>
                    <PlusIcon size={16} class="shrink-0" /><span class="truncate">{language.saveNewTogglePreset}</span>
                </button>
                <button class="{action} flex-1 min-w-0" onclick={importPreset}>
                    <UploadIcon size={16} class="shrink-0" /><span class="truncate">{language.importTogglePreset}</span>
                </button>
            </div>
            <div class="border-t border-darkborderc pt-3 flex flex-col gap-1">
                <div class="text-xs text-textcolor2 px-0.5">{language.toggleBinding}</div>
                <button
                    class="{action} w-full {DBState.db.defaultToggleValues !== undefined ? 'border-selected' : ''}"
                    onclick={manageDefaults}
                >
                    <SaveIcon size={16} class="shrink-0" />
                    <span class="truncate"
                        >{DBState.db.defaultToggleValues === undefined
                            ? language.saveDefaultToggles
                            : language.defaultTogglesSaved}</span
                    >
                </button>
                <SwitchInput name={language.disableToggleBinding} bind:check={DBState.db.disableToggleBinding} />
            </div>
        </div>
    </div>
    <button class="absolute inset-0 cursor-default" aria-label={language.cancel} onclick={dismiss}></button>
</div>
