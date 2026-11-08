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
    import SettingToggle from './SettingToggle.svelte'

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

<div class="fixed inset-0 z-[1300] flex items-center justify-center bg-black/60 p-4">
    <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="plugin-value-assign-title"
        data-plugin-value-assign
        class="flex max-h-[90vh] w-full max-w-2xl flex-col rounded-xl border border-darkborderc bg-darkbg text-textcolor"
    >
        <h3 id="plugin-value-assign-title" class="shrink-0 px-5 pt-5 text-lg font-bold">{strings.stageTitle}</h3>
        <!-- Only the list scrolls, so the title and the closing actions stay reachable. -->
        <div class="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-5 py-3">
            <p class="text-sm text-subtext">{strings.stageDescription}</p>
            {#if !automatic}
                <div class="rounded-md border border-darkborderc px-3 py-2 text-[13px] leading-normal text-subtext">
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
            <div>
                <SettingToggle bind:checked={automatic} label={strings.autoAssign} showLabel />
                <p class="mt-0.5 pl-7 text-xs text-subtext">{strings.autoAssignHelp}</p>
            </div>
        </div>
        <div class="flex shrink-0 flex-wrap justify-end gap-2 px-5 pt-1 pb-5">
            <SettingButton variant="secondary" onclick={() => onchoose({ assignments: [], automatic })}>
                {strings.skipAll}
            </SettingButton>
            <SettingButton onclick={apply}>{strings.applyAndContinue}</SettingButton>
        </div>
    </div>
</div>
