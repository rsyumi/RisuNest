<script lang="ts">
    import BanIcon from '@lucide/svelte/icons/ban'
    import CheckIcon from '@lucide/svelte/icons/check'
    import { language } from 'src/lang'
    import SegmentedButtons from 'src/lib/Setting/RisuNest/SegmentedButtons.svelte'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
    import SettingToggle from 'src/lib/Setting/RisuNest/SettingToggle.svelte'
    import OptionInput from 'src/lib/UI/GUI/OptionInput.svelte'
    import SelectInput from 'src/lib/UI/GUI/SelectInput.svelte'
    import { backupKindLabel, fillTemplate, formatBytes } from 'src/ts/gui/nativeFileJobDialogModel'
    import { importIntakeRefusalText } from 'src/ts/importIntake/refusalText'
    import { importSheet, resolveImportSheet, type ActiveImportSheet } from 'src/ts/importIntake/sheetStore'
    import type { ImportIntakeItem, ImportIntakeLorebookTarget, ImportSheetRowDecision } from 'src/ts/importIntake/types'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'

    type CharxChoice = NonNullable<ImportSheetRowDecision['charxAs']>
    type LorebookChoice = ImportIntakeLorebookTarget['type']

    // Choices made in the open sheet. Row ids repeat across deliveries, so a new sheet starts over from its defaults.
    let choicesOf: ActiveImportSheet | null = null
    let included = $state<Record<string, boolean>>({})
    let charxChoices = $state<Record<string, CharxChoice>>({})
    let lorebookChoices = $state<Record<string, LorebookChoice>>({})
    let installs = $state<Record<string, 'installing' | 'installed'>>({})

    $effect.pre(() => {
        const sheet = $importSheet
        if (sheet === choicesOf) return
        choicesOf = sheet
        included = {}
        charxChoices = {}
        lorebookChoices = {}
        installs = {}
    })

    const request = $derived($importSheet?.request)
    const items = $derived(request?.items ?? [])
    const importable = $derived(items.filter(isImportable))
    const showInclude = $derived(importable.length > 1)
    const chosen = $derived(importable.filter(item => !showInclude || (included[item.id] ?? true)))
    // The plugin dialog sits above the sheet, so Escape and Back belong to it until it settles.
    const installing = $derived(Object.values(installs).includes('installing'))

    function isImportable(item: ImportIntakeItem): boolean {
        return !item.refusal && item.kind !== 'plugin'
    }

    function isCharx(item: ImportIntakeItem): boolean {
        return item.kind === 'character' && item.format === 'charx'
    }

    function charxOf(item: ImportIntakeItem): CharxChoice {
        return charxChoices[item.id] ?? request?.defaults[item.id]?.charxAs ?? (item.moduleHint ? 'module' : 'character')
    }

    function lorebookOf(item: ImportIntakeItem): LorebookChoice {
        const choice = lorebookChoices[item.id] ?? request?.defaults[item.id]?.lorebookTarget?.type
            ?? (request?.lorebookCharacter ? 'character' : 'new-module')
        return choice === 'character' && request?.lorebookCharacter ? 'character' : 'new-module'
    }

    function decisionOf(item: ImportIntakeItem): ImportSheetRowDecision {
        const decision: ImportSheetRowDecision = { id: item.id }
        if (isCharx(item)) decision.charxAs = charxOf(item)
        if (item.kind === 'lorebook') {
            const character = request?.lorebookCharacter
            decision.lorebookTarget = lorebookOf(item) === 'character' && character
                ? { type: 'character', chaId: character.chaId, name: character.name }
                : { type: 'new-module' }
        }
        return decision
    }

    function kindText(item: ImportIntakeItem): string {
        switch (item.kind) {
            case 'character':
                return isCharx(item) ? language.risuNest.importIntake.kindCharx : language.character
            case 'module':
                return language.module
            case 'lorebook':
                return language.loreBook
            case 'preset':
                return language.presets
            case 'plugin':
                return language.plugin
            case 'backup':
                return item.format === 'portable' || item.format === 'block-risu-save' || item.format === 'local-backup'
                    ? backupKindLabel(item.format) : ''
            default:
                return ''
        }
    }

    function rowDetail(item: ImportIntakeItem): string {
        const size = item.bytes === undefined ? '' : formatBytes(item.bytes)
        let parts: string[]
        if (item.refusal) {
            parts = [item.kind === 'backup' ? kindText(item) : '', size, importIntakeRefusalText(item.refusal)]
        } else if (item.kind === 'plugin') {
            const header = item.plugin?.header
            const name = [header?.displayName || header?.name || item.name, header?.version ?? ''].filter(Boolean).join(' ')
            parts = [kindText(item), name, size]
        } else {
            parts = [kindText(item), size]
        }
        return parts.filter(Boolean).join(' · ')
    }

    async function install(item: ImportIntakeItem): Promise<void> {
        const sheet = $importSheet
        if (!sheet || installs[item.id]) return
        installs[item.id] = 'installing'
        let installed = false
        try {
            installed = await sheet.request.installPlugin(item)
        } catch (error) {
            console.error(error)
        }
        if (choicesOf !== sheet) return
        if (installed) installs[item.id] = 'installed'
        else delete installs[item.id]
    }

    function cancel(): void {
        if (!installing) resolveImportSheet(null)
    }

    function importChosen(): void {
        if (installing || chosen.length === 0) return
        resolveImportSheet(chosen.map(decisionOf))
    }
</script>

{#snippet fileText(name: string, detail: string, muted: boolean)}
    <span class="min-w-0">
        <span class="block text-sm break-words" class:text-subtext={muted}>{name}</span>
        <span class="block text-xs text-subtext">{detail}</span>
    </span>
{/snippet}

{#if $importSheet && request}
    {#key $importSheet}
        <div class="fixed inset-0 z-[1000] flex items-center justify-center bg-black/60 p-4">
            <button type="button" tabindex="-1" class="absolute inset-0" aria-label={importable.length === 0 ? language.risuNest.importDialog.close : language.cancel} onclick={cancel}></button>
            <div
                role="dialog"
                aria-modal="true"
                aria-labelledby="import-intake-sheet-title"
                data-testid="import-intake-sheet"
                class="relative flex max-h-[90dvh] w-full max-w-2xl flex-col rounded-lg border border-darkborderc bg-darkbg text-textcolor"
                use:modalNavigation={{ close: cancel, leaveEscape: () => installing }}>
                <header class="flex shrink-0 items-baseline justify-between gap-3 px-5 pt-5">
                    <h2 id="import-intake-sheet-title" class="text-lg font-bold">{language.risuNest.importDialog.titleImport}</h2>
                    <span class="shrink-0 text-sm text-subtext">{fillTemplate(language.risuNest.importDialog.batchFiles, items.length)}</span>
                </header>
                <div class="@container flex min-h-0 flex-1 flex-col overflow-y-auto px-5 py-3">
                    <ul class="flex flex-col divide-y divide-darkborderc rounded-lg border border-darkborderc">
                        {#each items as item, index (item.id)}
                            <li class="flex flex-col gap-2 px-3 py-2.5" data-import-row={item.id}>
                                <div class="flex flex-wrap items-center gap-x-3 gap-y-2">
                                    {#if item.refusal}
                                        <div class="flex min-w-0 flex-1 items-start gap-2">
                                            <BanIcon size={20} class="mt-0.5 shrink-0 text-subtext" aria-hidden="true" />
                                            {@render fileText(item.name, rowDetail(item), true)}
                                        </div>
                                    {:else if showInclude && isImportable(item)}
                                        <div class="flex min-w-0 flex-1 flex-col">
                                            <SettingToggle
                                                showLabel
                                                label={item.name}
                                                aria-describedby="import-intake-detail-{index}"
                                                checked={included[item.id] ?? true}
                                                onchange={checked => { included[item.id] = checked }}
                                            />
                                            <span id="import-intake-detail-{index}" class="block pl-7 text-xs text-subtext">{rowDetail(item)}</span>
                                        </div>
                                    {:else}
                                        <div class="flex min-w-0 flex-1 items-start gap-2">
                                            {#if showInclude}
                                                <span class="w-5 shrink-0" aria-hidden="true"></span>
                                            {/if}
                                            {@render fileText(item.name, rowDetail(item), false)}
                                        </div>
                                    {/if}
                                    {#if item.refusal}
                                        <!-- A refused file offers no control. -->
                                    {:else if item.kind === 'plugin'}
                                        {#if installs[item.id] === 'installed'}
                                            <span class="inline-flex items-center gap-1 text-sm text-subtext">
                                                <CheckIcon size={15} class="text-success-500" aria-hidden="true" />{language.risuNest.importIntake.installed}
                                            </span>
                                        {:else}
                                            <SettingButton variant="secondary" size="sm" busy={installs[item.id] === 'installing'} onclick={() => install(item)}>{language.risuNest.importIntake.install}</SettingButton>
                                        {/if}
                                    {:else if isCharx(item)}
                                        <div class="w-full @xl:w-auto">
                                            <SegmentedButtons
                                                role="radiogroup"
                                                label={fillTemplate(language.risuNest.importIntake.charxAsLabel, item.name)}
                                                value={charxOf(item)}
                                                options={[{ value: 'character', label: language.character }, { value: 'module', label: language.module }]}
                                                onchange={value => { charxChoices[item.id] = value }}
                                            />
                                        </div>
                                    {:else if item.kind === 'lorebook'}
                                        <div class="w-full @xl:w-auto">
                                            <SelectInput
                                                size="sm"
                                                className="w-full @xl:w-auto"
                                                ariaLabel={fillTemplate(language.risuNest.importIntake.destinationLabel, item.name)}
                                                value={lorebookOf(item)}
                                                onchange={event => { lorebookChoices[item.id] = event.currentTarget.value === 'character' ? 'character' : 'new-module' }}>
                                                {#if request.lorebookCharacter}
                                                    <OptionInput value="character">{fillTemplate(language.risuNest.importIntake.destinationCharacter, request.lorebookCharacter.name)}</OptionInput>
                                                {/if}
                                                <OptionInput value="new-module">{language.risuNest.importIntake.destinationNewModule}</OptionInput>
                                            </SelectInput>
                                        </div>
                                    {/if}
                                </div>
                            </li>
                        {/each}
                    </ul>
                </div>
                <footer class="flex shrink-0 flex-wrap items-center justify-end gap-2 px-5 pt-1 pb-5">
                    {#if importable.length === 0}
                        <SettingButton onclick={cancel}>{language.risuNest.importDialog.close}</SettingButton>
                    {:else}
                        <SettingButton variant="secondary" onclick={cancel}>{language.cancel}</SettingButton>
                        <SettingButton disabled={chosen.length === 0} onclick={importChosen}>{language.risuNest.importIntake.importAction}</SettingButton>
                    {/if}
                </footer>
            </div>
        </div>
    {/key}
{/if}
