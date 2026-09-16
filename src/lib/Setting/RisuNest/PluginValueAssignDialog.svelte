<script lang="ts">
    import { untrack } from 'svelte'
    import { language } from 'src/lang'
    import { UNOWNED_PLUGIN_OWNER } from 'src/ts/plugins/pluginOwner'
    import type { PluginDataItem } from 'src/ts/plugins/pluginDataInventory'
    import type {
        NativeStagedPluginChoice,
        NativeStagedPluginValue,
    } from 'src/ts/storage/nativeFileJobs'
    import PluginDataManager from './PluginDataManager.svelte'
    import SettingButton from './SettingButton.svelte'

    interface Props {
        values: NativeStagedPluginValue[]
        /** Plugins the imported save carries, which are the owners to choose from. */
        pluginNames: string[]
        /** Answers an attempt over the same save gave before it was cancelled. */
        remembered?: NativeStagedPluginChoice | null
        onchoose: (choice: NativeStagedPluginChoice | null) => void
    }

    let { values, pluginNames, remembered = null, onchoose }: Props = $props()

    const strings = language.risuNest.pluginData.import
    let automatic = $state(untrack(() => remembered?.automatic ?? true))
    let assignments: { owner: string; items: PluginDataItem[] }[] = $state([])

    const staged: PluginDataItem[] = $derived(
        values.map((value) => ({
            owner: UNOWNED_PLUGIN_OWNER,
            key: value.key,
            valueType: value.valueType,
            byteSize: value.byteSize,
            automatic: false,
        })),
    )

    function apply(): void {
        onchoose({
            assignments: assignments.map((assignment) => ({
                owner: assignment.owner,
                keys: assignment.items.map((item) => item.key),
            })),
            automatic,
        })
    }
</script>

<div class="fixed inset-0 z-[1300] flex items-center justify-center bg-black/65 p-4">
    <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="plugin-value-assign-title"
        data-plugin-value-assign
        class="flex max-h-[90vh] w-full max-w-2xl flex-col gap-3 overflow-y-auto rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor"
    >
        <h3 id="plugin-value-assign-title" class="text-lg font-bold">{strings.stageTitle}</h3>
        <p class="text-sm text-textcolor2">{strings.stageDescription}</p>
        {#if !automatic}
            <div class="rounded-md border border-darkborderc px-3 py-2 text-[13px] leading-normal text-textcolor2">
                {strings.autoAssignOffNotice}
            </div>
        {/if}
        <div class="rounded-lg border border-darkborderc">
            <PluginDataManager
                place="import"
                {staged}
                {pluginNames}
                initialAssignments={remembered?.assignments}
                onselectionchange={(chosen) => { assignments = chosen }}
            />
        </div>
        <label class="flex items-start gap-2 text-sm">
            <input type="checkbox" class="mt-1" bind:checked={automatic} />
            <span>
                {strings.autoAssign}
                <span class="mt-0.5 block text-xs text-textcolor2">{strings.autoAssignHelp}</span>
            </span>
        </label>
        <div class="flex flex-wrap justify-end gap-2">
            <SettingButton variant="secondary" onclick={() => onchoose({ assignments: [], automatic })}>
                {strings.skipAll}
            </SettingButton>
            <SettingButton onclick={apply}>{strings.applyAndContinue}</SettingButton>
        </div>
    </div>
</div>
