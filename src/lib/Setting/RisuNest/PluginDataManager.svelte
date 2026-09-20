<script lang="ts">
    import { onMount } from 'svelte'
    import { language } from 'src/lang'
    import { isTauri } from 'src/ts/platform'
    import { DBState } from 'src/ts/stores.svelte'
    import { formatRisuNestStorageBytes } from 'src/ts/storage/risuNestStorageDashboard'
    import { UNOWNED_PLUGIN_OWNER, isUnownedPluginOwner } from 'src/ts/plugins/pluginOwner'
    import {
        assignPluginDataItems,
        collidingPluginDataKeys,
        deletePluginDataItems,
        filterPluginDataItems,
        groupPluginDataByPrefix,
        listPluginDataItems,
        pluginDataAssignmentPrefill,
        pluginDataItemId,
        pluginDataOwnerBuckets,
        readPluginDataValue,
        totalPluginDataBytes,
        type PluginAssignCollision,
        type PluginDataAssignmentChoice,
        type PluginDataItem,
        type PluginDataScope,
    } from 'src/ts/plugins/pluginDataInventory'
    import { readLocalDataParticipation } from 'src/ts/storage/localDataSections'
    import SettingButton from './SettingButton.svelte'

    interface Props {
        /** The import stage assigns staged values and never shows the device scope. */
        place?: 'settings' | 'import'
        /** Values still being staged, which are not in the store yet. */
        staged?: PluginDataItem[]
        /** Plugins to offer as owners, when the installed list is not the right one. */
        pluginNames?: string[]
        /** Assignments to open on, which an import that was cancelled kept. */
        initialAssignments?: readonly PluginDataAssignmentChoice[]
        /** Selection the import stage reads back when the person continues. */
        onselectionchange?: (assignments: { owner: string; items: PluginDataItem[] }[]) => void
    }

    let {
        place = 'settings',
        staged,
        pluginNames,
        initialAssignments,
        onselectionchange,
    }: Props = $props()

    const strings = language.risuNest.pluginData
    const assignStrings = strings.assign

    let scope: PluginDataScope = $state('library')
    /** Whether this device takes the plugin section into synchronization. */
    let deviceSynced = $state(false)
    let items: PluginDataItem[] = $state([])
    let loading = $state(false)
    let busy = $state(false)
    let ownerFilter: string | null = $state(null)
    let automaticOnly = $state(false)
    let keyQuery = $state('')
    let valueQuery = $state('')
    let groupByPrefix = $state(true)
    let values = $state(new Map<string, string>())
    let selected = $state(new Set<string>())
    let groupOwners = $state(new Map<string, string>())
    let openItem: PluginDataItem | null = $state(null)
    let openValue = $state('')
    let confirming: {
        owner: string
        items: PluginDataItem[]
        prefix: string | null
    } | null = $state(null)
    let collision: {
        owner: string
        items: PluginDataItem[]
        colliding: string[]
    } | null = $state(null)
    let reloadPrompt = $state(false)

    const installedPlugins = $derived(
        (pluginNames ?? (DBState.db?.plugins ?? []).map((plugin) => plugin.name)).filter(
            (name) => name.length > 0,
        ),
    )
    const buckets = $derived(pluginDataOwnerBuckets(items))
    const visible = $derived(
        filterPluginDataItems(
            items,
            { owner: ownerFilter, automaticOnly, key: keyQuery, value: valueQuery },
            values,
        ),
    )
    const unknownSelected = $derived(
        ownerFilter !== null && isUnownedPluginOwner(ownerFilter),
    )
    const groups = $derived(groupByPrefix ? groupPluginDataByPrefix(visible) : [])
    const usage = $derived(
        ownerFilter !== null && !isUnownedPluginOwner(ownerFilter)
            ? strings.usageSingleOwner
                  .replace('{0}', ownerFilter)
                  .replace('{1}', formatRisuNestStorageBytes(totalPluginDataBytes(visible)))
            : buckets
                  .map(
                      (bucket) =>
                          `${ownerLabel(bucket.owner)} ${formatRisuNestStorageBytes(bucket.byteSize)}`,
                  )
                  .join(' · '),
    )
    const selectedItems = $derived(visible.filter((item) => selected.has(pluginDataItemId(item))))

    function ownerLabel(owner: string): string {
        return isUnownedPluginOwner(owner) ? strings.ownerUnknown : owner
    }

    function typeLabel(item: PluginDataItem): string {
        if (item.space !== undefined) {
            return item.space === 'json' ? strings.spaceJson : strings.spaceString
        }
        return item.valueType === 'string' ? strings.typeText : strings.typeJson
    }

    async function load(): Promise<void> {
        if (loading) return
        loading = true
        try {
            items = await listPluginDataItems(scope)
            values = new Map()
            selected = new Set()
        } finally {
            loading = false
        }
    }

    async function loadValuesForSearch(): Promise<void> {
        if (valueQuery.trim().length === 0) return
        const pending = filterPluginDataItems(
            items,
            { owner: ownerFilter, automaticOnly, key: keyQuery, value: '' },
            values,
        ).filter((item) => !values.has(pluginDataItemId(item)))
        if (pending.length === 0) return
        const loaded = new Map(values)
        for (const item of pending) {
            loaded.set(pluginDataItemId(item), (await readPluginDataValue(item)) ?? '')
        }
        values = loaded
    }

    /** The choice lives on the same settings page, so it is read again here. */
    async function loadParticipation(): Promise<void> {
        if (place !== 'settings') return
        try {
            const rows = await readLocalDataParticipation()
            deviceSynced = rows.some(
                (row) => row.section === 'local-plugins' && row.participating,
            )
        } catch {
            deviceSynced = false
        }
    }

    async function chooseScope(next: PluginDataScope): Promise<void> {
        if (scope === next) return
        scope = next
        ownerFilter = null
        automaticOnly = false
        if (next === 'device') await loadParticipation()
        await load()
    }

    function chooseOwner(owner: string | null): void {
        ownerFilter = owner
        automaticOnly = false
        selected = new Set()
    }

    function chooseAutomatic(): void {
        ownerFilter = null
        automaticOnly = !automaticOnly
        selected = new Set()
    }

    function toggle(item: PluginDataItem): void {
        const id = pluginDataItemId(item)
        const next = new Set(selected)
        if (next.has(id)) next.delete(id)
        else next.add(id)
        selected = next
        publishSelection()
    }

    function toggleGroup(groupItems: readonly PluginDataItem[], on: boolean): void {
        const next = new Set(selected)
        for (const item of groupItems) {
            const id = pluginDataItemId(item)
            if (on) next.add(id)
            else next.delete(id)
        }
        selected = next
        publishSelection()
    }

    function groupKey(prefix: string | null): string {
        return prefix ?? '\u0000ungrouped'
    }

    function setGroupOwner(prefix: string | null, owner: string): void {
        const next = new Map(groupOwners)
        if (owner.length === 0) next.delete(groupKey(prefix))
        else next.set(groupKey(prefix), owner)
        groupOwners = next
        publishSelection()
    }

    function publishSelection(): void {
        if (!onselectionchange) return
        const assignments: { owner: string; items: PluginDataItem[] }[] = []
        for (const group of groups) {
            const owner = groupOwners.get(groupKey(group.prefix))
            if (!owner) continue
            const chosen = group.items.filter((item) => selected.has(pluginDataItemId(item)))
            if (chosen.length > 0) assignments.push({ owner, items: chosen })
        }
        onselectionchange(assignments)
    }

    async function open(item: PluginDataItem): Promise<void> {
        openItem = item
        openValue = (await readPluginDataValue(item)) ?? ''
    }

    /**
     * Every deletion is confirmed. Deleting the whole list or everything shown asks a second
     * time, since one filter change away it is the plugin's entire store.
     */
    async function confirmRemoval(
        targets: readonly PluginDataItem[],
        bulk: 'all' | 'visible' | null,
    ): Promise<boolean> {
        const { alertConfirm } = await import('src/ts/alert')
        const count = String(targets.length)
        if (bulk === 'all') {
            return (
                (await alertConfirm(strings.deleteAllConfirm.replace('{0}', count))) &&
                (await alertConfirm(strings.deleteAllConfirmFinal))
            )
        }
        if (bulk === 'visible') {
            return (
                (await alertConfirm(strings.deleteVisibleConfirm.replace('{0}', count))) &&
                (await alertConfirm(strings.deleteVisibleConfirmFinal))
            )
        }
        return alertConfirm(
            targets.length === 1
                ? strings.deleteConfirmOne
                : strings.deleteConfirmSelected.replace('{0}', count),
        )
    }

    async function remove(
        targets: readonly PluginDataItem[],
        bulk: 'all' | 'visible' | null = null,
    ): Promise<void> {
        if (targets.length === 0 || busy) return
        if (!(await confirmRemoval(targets, bulk))) return
        busy = true
        try {
            await deletePluginDataItems(targets)
        } finally {
            busy = false
        }
        openItem = null
        await load()
    }

    function startAssign(
        owner: string,
        targets: readonly PluginDataItem[],
        prefix: string | null,
    ): void {
        if (owner.length === 0 || targets.length === 0 || busy) return
        confirming = { owner, items: [...targets], prefix }
    }

    async function confirmAssign(): Promise<void> {
        const pending = confirming
        if (!pending) return
        confirming = null
        const colliding = await collidingPluginDataKeys(
            pending.owner,
            pending.items.map((item) => item.key),
        )
        if (colliding.length > 0) {
            collision = { owner: pending.owner, items: pending.items, colliding }
            return
        }
        await applyAssign(pending.owner, pending.items, 'defer')
    }

    async function applyAssign(
        owner: string,
        targets: readonly PluginDataItem[],
        choice: PluginAssignCollision,
    ): Promise<void> {
        busy = true
        try {
            await assignPluginDataItems(targets, owner, choice)
        } finally {
            busy = false
        }
        collision = null
        await load()
        reloadPrompt = true
    }

    /** The automatic bucket has no bundles, so the plugin is chosen once here. */
    async function reassignVisible(): Promise<void> {
        if (visible.length === 0 || installedPlugins.length === 0 || busy) return
        const { alertSelect } = await import('src/ts/alert')
        const index = Number(await alertSelect(installedPlugins, strings.choosePlugin))
        const owner = installedPlugins[index]
        if (!owner) return
        startAssign(owner, visible, null)
    }

    async function reloadPlugins(): Promise<void> {
        reloadPrompt = false
        const { loadPlugins } = await import('src/ts/plugins/plugins.svelte')
        await loadPlugins()
    }

    /** Opens on the answers an import kept, so they are confirmed, not redone. */
    function applyInitialAssignments(): void {
        if (!staged || !initialAssignments?.length) return
        const prefill = pluginDataAssignmentPrefill(
            items,
            initialAssignments,
            installedPlugins,
        )
        selected = new Set(prefill.selectedIds)
        groupOwners = new Map(
            prefill.groupOwners.map((group) => [groupKey(group.prefix), group.owner]),
        )
        publishSelection()
    }

    onMount(() => {
        if (staged) items = [...staged]
        else void load()
        void loadParticipation()
        applyInitialAssignments()
    })
</script>

{#snippet ownerChips()}
    <div class="flex flex-wrap items-center gap-1.5">
        {#if buckets.length > 0}
            <span class="text-xs text-textcolor2">{strings.ownerFilterTitle}</span>
        {/if}
        {#each buckets as bucket (bucket.owner)}
            <button
                type="button"
                data-plugin-data-owner={bucket.owner}
                aria-pressed={ownerFilter === bucket.owner}
                class="rounded-full border px-2.5 py-0.5 text-xs transition-colors duration-200 {ownerFilter === bucket.owner ? 'border-borderc bg-selected text-textcolor' : 'border-darkborderc text-textcolor2 hover:bg-selected'} {isUnownedPluginOwner(bucket.owner) ? 'border-danger-400/50 text-danger-400' : ''}"
                onclick={() => chooseOwner(ownerFilter === bucket.owner ? null : bucket.owner)}
            >{ownerLabel(bucket.owner)} <span class="tabular-nums opacity-70">{bucket.count}</span></button>
        {/each}
        {#if items.some((item) => item.automatic)}
            <button
                type="button"
                data-plugin-data-automatic
                aria-pressed={automaticOnly}
                class="rounded-full border px-2.5 py-0.5 text-xs transition-colors duration-200 {automaticOnly ? 'border-borderc bg-selected text-textcolor' : 'border-darkborderc text-textcolor2 hover:bg-selected'}"
                onclick={chooseAutomatic}
            >{strings.ownerAutoAssigned} <span class="tabular-nums opacity-70">{items.filter((item) => item.automatic).length}</span></button>
        {/if}
    </div>
{/snippet}

<div class="flex min-w-0 flex-col gap-3 px-4 py-3" data-plugin-data-manager>
    {#if place === 'settings' && isTauri}
        <div class="flex flex-wrap items-center gap-2.5">
            <div class="inline-flex overflow-hidden rounded-md border border-darkborderc">
                <button
                    type="button"
                    data-plugin-data-scope="library"
                    aria-pressed={scope === 'library'}
                    class="px-3 py-1 text-sm {scope === 'library' ? 'bg-selected text-textcolor' : 'text-textcolor2'}"
                    onclick={() => chooseScope('library')}
                >{strings.scopeAllDevices}</button>
                <button
                    type="button"
                    data-plugin-data-scope="device"
                    aria-pressed={scope === 'device'}
                    class="px-3 py-1 text-sm {scope === 'device' ? 'bg-selected text-textcolor' : 'text-textcolor2'}"
                    onclick={() => chooseScope('device')}
                >{strings.scopeThisDevice}</button>
            </div>
            <span class="text-xs text-textcolor2">
                {scope === 'library' ? strings.scopeAllDevicesHelp : strings.scopeThisDeviceHelp}
            </span>
        </div>
    {/if}

    {#if place === 'settings'}
        <div class="flex flex-col gap-2 @md:flex-row">
            <input
                type="search"
                bind:value={keyQuery}
                placeholder={strings.searchKey}
                aria-label={strings.searchKey}
                class="w-full min-w-0 flex-1 rounded-md border border-darkborderc bg-bgcolor px-2.5 py-1 text-sm"
            />
            <input
                type="search"
                bind:value={valueQuery}
                onchange={loadValuesForSearch}
                placeholder={strings.searchValue}
                aria-label={strings.searchValue}
                class="w-full min-w-0 flex-1 rounded-md border border-darkborderc bg-bgcolor px-2.5 py-1 text-sm"
            />
        </div>
        {@render ownerChips()}
        {#if usage.length > 0}
            <p class="text-xs text-textcolor2">{usage}</p>
        {/if}
    {/if}

    {#if unknownSelected || place === 'import'}
        <div class="rounded-md border border-danger-400/50 bg-danger-400/5 px-3 py-2 text-[13px] leading-normal">
            {#if place === 'import'}
                <p>{language.risuNest.pluginData.import.warning}</p>
            {:else}
                <p class="font-semibold">{strings.unknownBucketTitle}</p>
                <p class="mt-1 text-textcolor2">{strings.unknownBucketHelp}</p>
            {/if}
        </div>
    {:else if automaticOnly}
        <div class="rounded-md border border-darkborderc px-3 py-2 text-[13px] leading-normal text-textcolor2">
            {strings.autoBucketHelp}
        </div>
    {/if}

    {#if scope === 'device' && place === 'settings' && !deviceSynced}
        <div class="rounded-md border border-darkborderc px-3 py-2 text-[13px] leading-normal">
            <span class="font-semibold">{strings.deviceNotSyncedTitle}</span>
            <span class="text-textcolor2">{strings.deviceNotSyncedHelp}</span>
        </div>
    {/if}

    {#if unknownSelected || place === 'import'}
        <div class="flex flex-wrap items-center justify-between gap-2">
            <label class="flex items-center gap-1.5 text-sm">
                <input type="checkbox" bind:checked={groupByPrefix} />
                {strings.groupByPrefix}
            </label>
            {#if place === 'settings'}
                <div class="flex gap-1.5">
                    <SettingButton variant="secondary" disabled={selectedItems.length === 0} busy={busy} onclick={() => remove(selectedItems)}>{strings.deleteSelected}</SettingButton>
                </div>
            {:else}
                <span class="text-xs text-textcolor2">
                    {language.risuNest.pluginData.import.summary
                        .replace('{0}', String(visible.length))
                        .replace('{1}', formatRisuNestStorageBytes(totalPluginDataBytes(visible)))}
                </span>
            {/if}
        </div>
        {#each groups as group (groupKey(group.prefix))}
            {@const chosen = group.items.filter((item) => selected.has(pluginDataItemId(item)))}
            <div class="rounded-md border border-darkborderc" data-plugin-data-group={group.prefix ?? ''}>
                <div class="flex flex-wrap items-center gap-2 px-3 py-2">
                    <input
                        type="checkbox"
                        aria-label={group.prefix ?? strings.ungrouped.replace('{0}', String(group.items.length)).replace('{1}', '')}
                        checked={chosen.length === group.items.length}
                        indeterminate={chosen.length > 0 && chosen.length < group.items.length}
                        onchange={(event) => toggleGroup(group.items, event.currentTarget.checked)}
                    />
                    <span class="min-w-0 text-sm">
                        {group.prefix === null
                            ? strings.ungrouped
                                  .replace('{0}', String(group.items.length))
                                  .replace('{1}', formatRisuNestStorageBytes(group.byteSize))
                            : strings.prefixGroup
                                  .replace('{0}', group.prefix)
                                  .replace('{1}', String(group.items.length))
                                  .replace('{2}', formatRisuNestStorageBytes(group.byteSize))}
                    </span>
                    {#if installedPlugins.length > 0}
                        <select
                            class="ml-auto min-w-0 max-w-full rounded-md border border-darkborderc bg-bgcolor px-2 py-1 text-sm"
                            aria-label={strings.choosePlugin}
                            value={groupOwners.get(groupKey(group.prefix)) ?? ''}
                            onchange={(event) => setGroupOwner(group.prefix, event.currentTarget.value)}
                        >
                            <option value="">{strings.choosePlugin}</option>
                            {#each installedPlugins as name (name)}
                                <option value={name}>{name}</option>
                            {/each}
                        </select>
                        {#if place === 'settings'}
                            <SettingButton
                                disabled={chosen.length === 0 || !groupOwners.get(groupKey(group.prefix))}
                                busy={busy}
                                onclick={() => startAssign(groupOwners.get(groupKey(group.prefix)) ?? '', chosen, group.prefix)}
                            >{strings.assignSelected}</SettingButton>
                        {/if}
                    {/if}
                </div>
                <div class="flex flex-wrap gap-x-3 gap-y-1 border-t border-darkborderc/55 px-3 py-1.5 font-mono text-xs text-textcolor2">
                    {#each group.items.slice(0, 4) as item (pluginDataItemId(item))}
                        <button type="button" class="hover:text-textcolor" onclick={() => toggle(item)}>{item.key}</button>
                    {/each}
                    {#if group.items.length > 4}
                        <span>{strings.andMore.replace('{0}', String(group.items.length - 4))}</span>
                    {/if}
                </div>
            </div>
        {/each}
    {/if}

    {#if place === 'settings'}
        <div class="flex flex-wrap items-center justify-between gap-2">
            <span class="text-sm tabular-nums text-textcolor2" data-plugin-data-count>
                {strings.countOfTotal
                    .replace('{0}', String(items.length))
                    .replace('{1}', String(visible.length))}
            </span>
            <div class="flex gap-1.5">
                {#if automaticOnly}
                    <SettingButton variant="secondary" disabled={visible.length === 0 || installedPlugins.length === 0} busy={busy} onclick={reassignVisible}>{strings.reassign}</SettingButton>
                {/if}
                <SettingButton variant="danger" disabled={visible.length === 0} busy={busy} onclick={() => remove(visible, visible.length === items.length ? 'all' : 'visible')}>
                    {visible.length === items.length
                        ? strings.deleteAll.replace('{0}', String(items.length))
                        : strings.deleteVisible.replace('{0}', String(visible.length))}
                </SettingButton>
                <SettingButton variant="secondary" busy={loading} onclick={load}>{strings.refresh}</SettingButton>
            </div>
        </div>
        {#if visible.length === 0}
            <p class="py-8 text-center text-sm text-textcolor2">{strings.empty}</p>
        {:else}
            <ul data-plugin-data-list class="max-h-[clamp(24rem,65dvh,52rem)] divide-y divide-darkborderc/55 overflow-y-auto rounded-md border border-darkborderc">
                {#each visible as item (pluginDataItemId(item))}
                    <li class="flex items-center gap-2 px-3 py-1.5 text-sm" data-plugin-data-row={item.key}>
                        <button type="button" class="min-w-0 flex-1 truncate text-left font-mono hover:text-textcolor" title={strings.openValue} onclick={() => open(item)}>{item.key}</button>
                        <span class="shrink-0 text-xs {isUnownedPluginOwner(item.owner) ? 'text-danger-400' : 'text-textcolor2'}">
                            {item.automatic
                                ? strings.autoAssignedTag.replace('{0}', ownerLabel(item.owner))
                                : ownerLabel(item.owner)}
                        </span>
                        <span class="shrink-0 text-xs text-textcolor2">{typeLabel(item)}</span>
                        <span class="shrink-0 text-xs tabular-nums text-textcolor2">{formatRisuNestStorageBytes(item.byteSize)}</span>
                        <button type="button" class="shrink-0 px-1 text-textcolor2 hover:text-danger-400" aria-label={language.remove} onclick={() => remove([item])}>✕</button>
                    </li>
                {/each}
            </ul>
        {/if}
    {/if}
</div>

{#if openItem}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/65 p-4">
        <div role="dialog" aria-modal="true" aria-label={openItem.key} class="flex w-full max-w-2xl flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="font-mono text-base break-all">{openItem.key}</h3>
            <div class="flex flex-wrap gap-x-4 gap-y-1 text-xs text-textcolor2">
                <span><b class="font-semibold">{strings.detailOwner}</b> {ownerLabel(openItem.owner)}</span>
                <span><b class="font-semibold">{strings.detailType}</b> {typeLabel(openItem)}</span>
                <span><b class="font-semibold">{strings.detailSize}</b> {formatRisuNestStorageBytes(openItem.byteSize)}</span>
                <span><b class="font-semibold">{strings.detailChars}</b> {openValue.length.toLocaleString()}</span>
            </div>
            <pre class="max-h-80 overflow-auto rounded-md border border-darkborderc bg-bgcolor p-3 font-mono text-xs whitespace-pre-wrap">{openValue}</pre>
            <div class="flex justify-end gap-2">
                <SettingButton variant="danger" busy={busy} onclick={() => remove(openItem ? [openItem] : [])}>{language.remove}</SettingButton>
                <SettingButton variant="secondary" onclick={() => { openItem = null }}>{language.risuNest.importDialog.close}</SettingButton>
            </div>
        </div>
    </div>
{/if}

{#if confirming}
    {@const pending = confirming}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/65 p-4">
        <div role="dialog" aria-modal="true" class="flex w-full max-w-md flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="text-base font-bold">{assignStrings.confirmTitle.replace('{0}', pending.owner)}</h3>
            {#if pending.prefix !== null}
                <p class="text-sm text-textcolor2">
                    {assignStrings.confirmSummary
                        .replace('{0}', pending.prefix)
                        .replace('{1}', String(pending.items.length))
                        .replace('{2}', formatRisuNestStorageBytes(totalPluginDataBytes(pending.items)))}
                </p>
            {/if}
            <p class="text-sm text-textcolor2">{assignStrings.confirmBody.replace('{0}', pending.owner)}</p>
            <div class="flex justify-end gap-2">
                <SettingButton variant="secondary" onclick={() => { confirming = null }}>{language.cancel}</SettingButton>
                <SettingButton busy={busy} onclick={confirmAssign}>{assignStrings.confirmAction}</SettingButton>
            </div>
        </div>
    </div>
{/if}

{#if collision}
    {@const pending = collision}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/65 p-4">
        <div role="dialog" aria-modal="true" class="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="text-base font-bold">{assignStrings.collisionTitle.replace('{0}', String(pending.colliding.length))}</h3>
            <p class="text-sm text-textcolor2">
                {assignStrings.collisionBody
                    .replace('{0}', pending.owner)
                    .replace('{1}', String(pending.items.length - pending.colliding.length))
                    .replace('{0}', pending.owner)}
            </p>
            <ul class="max-h-40 overflow-auto rounded-md border border-darkborderc font-mono text-xs">
                {#each pending.colliding as key (key)}
                    <li class="px-3 py-1">{key}</li>
                {/each}
            </ul>
            <div class="flex flex-col gap-2">
                <SettingButton variant="secondary" class="flex-col items-start gap-0" busy={busy} onclick={() => applyAssign(pending.owner, pending.items, 'replace')}>
                    <span>{assignStrings.collisionReplace}</span>
                    <span class="text-xs text-textcolor2">{assignStrings.collisionReplaceHelp}</span>
                </SettingButton>
                <SettingButton variant="secondary" class="flex-col items-start gap-0" busy={busy} onclick={() => applyAssign(pending.owner, pending.items, 'discard')}>
                    <span>{assignStrings.collisionDiscard}</span>
                    <span class="text-xs text-textcolor2">{assignStrings.collisionDiscardHelp.replace('{0}', String(pending.colliding.length))}</span>
                </SettingButton>
                <SettingButton variant="secondary" class="flex-col items-start gap-0" busy={busy} onclick={() => applyAssign(pending.owner, pending.items, 'defer')}>
                    <span>{assignStrings.collisionDefer}</span>
                    <span class="text-xs text-textcolor2">{assignStrings.collisionDeferHelp.replace('{0}', String(pending.colliding.length))}</span>
                </SettingButton>
            </div>
        </div>
    </div>
{/if}

{#if reloadPrompt}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/65 p-4">
        <div role="dialog" aria-modal="true" class="flex w-full max-w-md flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="text-base font-bold">{assignStrings.reloadTitle}</h3>
            <p class="text-sm text-textcolor2">{assignStrings.reloadBody}</p>
            <div class="flex justify-end gap-2">
                <SettingButton variant="secondary" onclick={() => { reloadPrompt = false }}>{assignStrings.reloadLater}</SettingButton>
                <SettingButton onclick={reloadPlugins}>{assignStrings.reloadNow}</SettingButton>
            </div>
        </div>
    </div>
{/if}
