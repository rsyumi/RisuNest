<script lang="ts">
    import { onMount } from 'svelte'
    import { XIcon } from '@lucide/svelte'
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
        pluginDataItemId,
        pluginDataOwnerBuckets,
        readPluginDataValue,
        searchPluginDataValues,
        totalPluginDataBytes,
        type PluginAssignCollision,
        type PluginDataAssignmentChoice,
        type PluginDataItem,
        type PluginDataScope,
    } from 'src/ts/plugins/pluginDataInventory'
    import { readLocalDataParticipation } from 'src/ts/storage/localDataSections'
    import SegmentedButtons from './SegmentedButtons.svelte'
    import SettingButton from './SettingButton.svelte'
    import SettingToggle from './SettingToggle.svelte'
    import SelectInput from 'src/lib/UI/GUI/SelectInput.svelte'
    import TextInput from 'src/lib/UI/GUI/TextInput.svelte'

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
    const searchIds = {
        key: `plugin-data-key-${crypto.randomUUID()}`,
        value: `plugin-data-value-${crypto.randomUUID()}`,
    }
    const scopeOptions: { value: PluginDataScope; label: string }[] = [
        { value: 'library', label: strings.scopeAllDevices },
        { value: 'device', label: strings.scopeThisDevice },
    ]

    let scope: PluginDataScope = $state('library')
    /** Whether this device takes the plugin section into synchronization. */
    let deviceSynced = $state(false)
    let items: PluginDataItem[] = $state([])
    let itemsScope: PluginDataScope = $state('library')
    let loadGeneration = 0
    let searchGeneration = 0
    let searchLoading = $state(false)
    let searchFailed = $state(false)
    let loading = $state(false)
    let busy = $state(false)
    let ownerFilter: string | null = $state(null)
    let automaticOnly = $state(false)
    let keyQuery = $state('')
    let valueQuery = $state('')
    let groupByPrefix = $state(true)
    let values = $state(new Map<string, string>())
    let selected = $state(new Set<string>())
    let itemOwners = $state(new Map<string, string>())
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
        if (item.structureType !== undefined) return item.structureType
        if (item.space !== undefined) {
            return item.space === 'json' ? strings.spaceJson : strings.spaceString
        }
        return item.valueType === 'string' ? strings.typeText : strings.typeJson
    }

    async function load(): Promise<void> {
        const generation = ++loadGeneration
        const requested = scope
        searchGeneration += 1
        items = []
        selected = new Set()
        values = new Map()
        openItem = null
        loading = true
        try {
            const loaded = await listPluginDataItems(requested)
            if (generation !== loadGeneration) return
            itemsScope = requested
            items = loaded
        } finally {
            if (generation === loadGeneration) loading = false
        }
    }

    $effect(() => {
        const query = valueQuery.trim()
        const eligible = filterPluginDataItems(items,
            { owner: ownerFilter, automaticOnly, key: keyQuery, value: '' }, new Map())
        const generation = ++searchGeneration
        values = new Map()
        searchFailed = false
        const needsValues = query.length > 0 && eligible.length > 0
        searchLoading = needsValues
        if (!needsValues) return
        const timer = setTimeout(() => {
            void (async () => {
                try {
                    const matches = await searchPluginDataValues(eligible, query,
                        () => generation !== searchGeneration)
                    if (generation !== searchGeneration) return
                    values = new Map([...matches].map((id) => [id, query]))
                } catch {
                    if (generation === searchGeneration) searchFailed = true
                } finally {
                    if (generation === searchGeneration) searchLoading = false
                }
            })()
        }, 250)
        return () => {
            clearTimeout(timer)
            searchGeneration += 1
        }
    })

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
        scope = next
        ownerFilter = null
        automaticOnly = false
        const listing = load()
        if (next === 'device') void loadParticipation()
        await listing
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

    function groupLabel(group: {
        prefix: string | null
        items: readonly PluginDataItem[]
        byteSize: number
    }): string {
        return group.prefix === null
            ? strings.ungrouped
                  .replace('{0}', String(group.items.length))
                  .replace('{1}', formatRisuNestStorageBytes(group.byteSize))
            : strings.prefixGroup
                  .replace('{0}', group.prefix)
                  .replace('{1}', String(group.items.length))
                  .replace('{2}', formatRisuNestStorageBytes(group.byteSize))
    }

    function groupOwner(groupItems: readonly PluginDataItem[]): string {
        const owner = itemOwners.get(pluginDataItemId(groupItems[0])) ?? ''
        return groupItems.every((item) => (itemOwners.get(pluginDataItemId(item)) ?? '') === owner) ? owner : ''
    }

    function setItemOwners(groupItems: readonly PluginDataItem[], owner: string): void {
        const next = new Map(itemOwners)
        for (const item of groupItems) {
            if (owner.length === 0) next.delete(pluginDataItemId(item))
            else next.set(pluginDataItemId(item), owner)
        }
        itemOwners = next
        publishSelection()
    }

    function publishSelection(): void {
        if (!onselectionchange) return
        const assignments = new Map<string, PluginDataItem[]>()
        for (const item of items) {
            const id = pluginDataItemId(item)
            const owner = itemOwners.get(id)
            if (!owner || !selected.has(id)) continue
            const chosen = assignments.get(owner) ?? []
            chosen.push(item)
            assignments.set(owner, chosen)
        }
        onselectionchange([...assignments].map(([owner, items]) => ({ owner, items })))
    }

    async function open(item: PluginDataItem): Promise<void> {
        openItem = item
        openValue = ''
        if (item.structureType !== undefined) return
        const generation = loadGeneration
        const value = (await readPluginDataValue(item)) ?? ''
        if (generation === loadGeneration && openItem === item) openValue = value
    }

    async function confirmRemoval(
        targets: readonly PluginDataItem[],
        bulk: 'all' | 'visible' | null,
    ): Promise<boolean> {
        const { alertConfirm, alertCheckboxConfirm } = await import('src/ts/alert')
        const count = String(targets.length)
        if (bulk) {
            return (await alertCheckboxConfirm({
                title: (bulk === 'all' ? strings.deleteAllConfirm : strings.deleteVisibleConfirm).replace('{0}', count),
                description: bulk === 'all' ? strings.deleteAllConfirmFinal : strings.deleteVisibleConfirmFinal,
                checkboxLabel: language.checkboxConfirmation.pluginDataDeletion,
                actionLabel: language.risuNest.plugins.removeAction,
                cancelLabel: language.cancel,
                requireChecked: true,
            })).confirmed
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
        if (targets.length === 0 || busy || loading || itemsScope !== scope) return
        const expectedScope = itemsScope
        const generation = loadGeneration
        if (!(await confirmRemoval(targets, bulk))) return
        if (generation !== loadGeneration || expectedScope !== scope || busy) return
        busy = true
        try {
            await deletePluginDataItems(targets, expectedScope)
            if (targets.some((item) => installedPlugins.includes(item.owner))) reloadPrompt = true
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
        const owners = new Map<string, string>()
        for (const assignment of initialAssignments) {
            if (!installedPlugins.includes(assignment.owner)) continue
            for (const item of items) {
                const id = pluginDataItemId(item)
                if (assignment.keys.includes(item.key) && !owners.has(id)) owners.set(id, assignment.owner)
            }
        }
        selected = new Set(owners.keys())
        itemOwners = owners
        publishSelection()
    }

    onMount(() => {
        if (staged) items = [...staged]
        else void load()
        void loadParticipation()
        applyInitialAssignments()
    })
</script>

{#snippet assignmentRow(item: PluginDataItem)}
    {@const id = pluginDataItemId(item)}
    <div class="flex flex-wrap items-center gap-2 px-3 py-1.5" data-plugin-data-assignment={item.key}>
        <SettingToggle label={item.key} showLabel checked={selected.has(id)} onchange={() => toggle(item)} />
        {#if installedPlugins.length > 0}
            <SelectInput size="sm" ariaLabel={strings.choosePlugin} value={itemOwners.get(id) ?? ''} onchange={(event) => setItemOwners([item], event.currentTarget.value)}>
                <option value="">{strings.choosePlugin}</option>
                {#each installedPlugins as name (name)}
                    <option value={name}>{name}</option>
                {/each}
            </SelectInput>
            {#if place === 'settings'}
                <SettingButton disabled={!selected.has(id) || !itemOwners.get(id) || loading} busy={busy} onclick={() => startAssign(itemOwners.get(id) ?? '', [item], null)}>{strings.assignSelected}</SettingButton>
            {/if}
        {/if}
    </div>
{/snippet}

{#snippet ownerChips()}
    <div class="flex flex-wrap items-center gap-1.5">
        {#if buckets.length > 0}
            <span class="text-xs text-subtext">{strings.ownerFilterTitle}</span>
        {/if}
        {#each buckets as bucket (bucket.owner)}
            <button
                type="button"
                data-plugin-data-owner={bucket.owner}
                aria-pressed={ownerFilter === bucket.owner}
                class="rounded-full border px-2.5 py-0.5 text-xs transition-colors duration-200 {ownerFilter === bucket.owner ? 'border-borderc bg-selected text-textcolor' : 'border-darkborderc text-subtext hover:bg-selected'} {isUnownedPluginOwner(bucket.owner) ? 'border-danger-400/50 text-danger-400' : ''}"
                onclick={() => chooseOwner(ownerFilter === bucket.owner ? null : bucket.owner)}
            >{ownerLabel(bucket.owner)} <span class="tabular-nums opacity-70">{bucket.count}</span></button>
        {/each}
        {#if items.some((item) => item.automatic)}
            <button
                type="button"
                data-plugin-data-automatic
                aria-pressed={automaticOnly}
                class="rounded-full border px-2.5 py-0.5 text-xs transition-colors duration-200 {automaticOnly ? 'border-borderc bg-selected text-textcolor' : 'border-darkborderc text-subtext hover:bg-selected'}"
                onclick={chooseAutomatic}
            >{strings.ownerAutoAssigned} <span class="tabular-nums opacity-70">{items.filter((item) => item.automatic).length}</span></button>
        {/if}
    </div>
{/snippet}

<div class="flex min-w-0 flex-col gap-3 px-4 py-3" data-plugin-data-manager>
    {#if place === 'settings' && isTauri}
        <div data-plugin-data-scope class="flex flex-wrap items-center gap-2.5">
            <SegmentedButtons
                value={scope}
                options={scopeOptions}
                label={strings.scopeTitle}
                onchange={(next) => void chooseScope(next)}
            />
            <span class="text-[13px] leading-normal text-subtext">
                {scope === 'library' ? strings.scopeAllDevicesHelp : strings.scopeThisDeviceHelp}
            </span>
        </div>
    {/if}

    {#if place === 'settings'}
        <div class="flex flex-col gap-2 @md:flex-row">
            <div class="min-w-0 flex-1">
                <label class="sr-only" for={searchIds.key}>{strings.searchKey}</label>
                <TextInput id={searchIds.key} size="sm" fullwidth bind:value={keyQuery} placeholder={strings.searchKey} />
            </div>
            <div class="min-w-0 flex-1">
                <label class="sr-only" for={searchIds.value}>{strings.searchValue}</label>
                <TextInput id={searchIds.value} size="sm" fullwidth bind:value={valueQuery} placeholder={strings.searchValue} />
            </div>
        </div>
        {@render ownerChips()}
        {#if usage.length > 0}
            <p class="text-xs text-subtext">{usage}</p>
        {/if}
    {/if}

    {#if unknownSelected || place === 'import'}
        <div class="rounded-md border border-danger-400/50 bg-danger-400/5 px-3 py-2 text-[13px] leading-normal">
            {#if place === 'import'}
                <p>{language.risuNest.pluginData.import.warning}</p>
            {:else}
                <p class="font-semibold">{strings.unknownBucketTitle}</p>
                <p class="mt-1 text-subtext">{strings.unknownBucketHelp}</p>
            {/if}
        </div>
    {:else if automaticOnly}
        <div class="rounded-md border border-darkborderc px-3 py-2 text-[13px] leading-normal text-subtext">
            {strings.autoBucketHelp}
        </div>
    {/if}

    {#if scope === 'device' && place === 'settings' && !deviceSynced}
        <div class="rounded-md border border-darkborderc px-3 py-2 text-[13px] leading-normal">
            <span class="font-semibold">{strings.deviceNotSyncedTitle}</span>
            <span class="text-subtext">{strings.deviceNotSyncedHelp}</span>
        </div>
    {/if}

    {#if unknownSelected || place === 'import'}
        <div class="flex flex-wrap items-center justify-between gap-2">
            <SettingToggle checked={groupByPrefix} onchange={(checked) => { groupByPrefix = checked; publishSelection() }} label={strings.groupByPrefix} showLabel />
            {#if place === 'settings'}
                <div class="flex gap-2">
                    <SettingButton variant="secondary" disabled={selectedItems.length === 0} busy={busy} onclick={() => remove(selectedItems)}>{strings.deleteSelected}</SettingButton>
                </div>
            {:else}
                <span class="text-xs text-subtext">
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
                    <SettingToggle
                        label={groupLabel(group)}
                        showLabel
                        checked={chosen.length === group.items.length}
                        indeterminate={chosen.length > 0 && chosen.length < group.items.length}
                        onchange={(checked) => toggleGroup(group.items, checked)}
                    />
                    {#if installedPlugins.length > 0}
                        <div class="ml-auto min-w-0">
                            <SelectInput
                                size="sm"
                                className="w-full"
                                ariaLabel={strings.choosePlugin}
                                value={groupOwner(group.items)}
                                onchange={(event) => setItemOwners(group.items, event.currentTarget.value)}
                            >
                                <option value="">{strings.choosePlugin}</option>
                                {#each installedPlugins as name (name)}
                                    <option value={name}>{name}</option>
                                {/each}
                            </SelectInput>
                        </div>
                        {#if place === 'settings'}
                            <SettingButton
                                disabled={chosen.length === 0 || !groupOwner(group.items) || loading}
                                busy={busy}
                                onclick={() => startAssign(groupOwner(group.items), chosen, group.prefix)}
                            >{strings.assignSelected}</SettingButton>
                        {/if}
                    {/if}
                </div>
                <div class="flex flex-wrap gap-x-3 gap-y-1 border-t border-darkborderc/55 px-3 py-1.5 font-mono text-xs text-subtext">
                    {#each group.items as item (pluginDataItemId(item))}
                        {@render assignmentRow(item)}
                    {/each}
                </div>
            </div>
        {/each}
        {#if !groupByPrefix}
            {#each visible as item (pluginDataItemId(item))}
                {@render assignmentRow(item)}
            {/each}
        {/if}
    {/if}

    {#if place === 'settings'}
        <div class="flex flex-wrap items-center justify-between gap-2">
            <span class="text-sm tabular-nums text-subtext" data-plugin-data-count>
                {strings.countOfTotal
                    .replace('{0}', String(items.length))
                    .replace('{1}', String(visible.length))}
            </span>
            <div class="flex flex-wrap gap-2">
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
        {#if loading || searchLoading}
            <p class="py-8 text-center text-sm text-subtext" role="status" aria-live="polite">{language.loading}</p>
        {:else if searchFailed}
            <p class="py-8 text-center text-sm text-subtext" role="alert">{language.error}</p>
        {:else if visible.length === 0}
            <p class="py-8 text-center text-sm text-subtext">{strings.empty}</p>
        {:else if !unknownSelected}
            <ul data-plugin-data-list class="max-h-[clamp(24rem,65dvh,52rem)] divide-y divide-darkborderc/55 overflow-y-auto rounded-md border border-darkborderc">
                {#each visible as item (pluginDataItemId(item))}
                    <!-- The key keeps the first line; the facts wrap under it on a narrow panel. -->
                    <li class="flex flex-wrap items-center gap-x-2 gap-y-1 px-3 py-1.5 text-sm" data-plugin-data-row={item.key}>
                        <button type="button" class="min-w-0 flex-[1_1_10rem] truncate text-left font-mono hover:text-textcolor" title={strings.openValue} onclick={() => open(item)}>{item.key}</button>
                        <span class="shrink-0 text-xs {isUnownedPluginOwner(item.owner) ? 'text-danger-400' : 'text-subtext'}">
                            {item.automatic
                                ? strings.autoAssignedTag.replace('{0}', ownerLabel(item.owner))
                                : ownerLabel(item.owner)}
                        </span>
                        <span class="shrink-0 text-xs text-subtext">{typeLabel(item)}</span>
                        <span class="shrink-0 text-xs tabular-nums text-subtext">{formatRisuNestStorageBytes(item.byteSize)}</span>
                        <button type="button" class="ml-auto shrink-0 px-1 text-subtext hover:text-danger-400" aria-label={language.remove} onclick={() => remove([item])}><XIcon size={16} aria-hidden="true" /></button>
                    </li>
                {/each}
            </ul>
        {/if}
    {/if}
</div>

{#if openItem}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/60 p-4">
        <div role="dialog" aria-modal="true" aria-label={openItem.key} class="flex w-full max-w-2xl flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="font-mono text-lg font-bold break-all">{openItem.key}</h3>
            <div class="flex flex-wrap gap-x-4 gap-y-1 text-xs text-subtext">
                <span><b class="font-semibold">{strings.detailOwner}</b> {ownerLabel(openItem.owner)}</span>
                <span><b class="font-semibold">{strings.detailType}</b> {typeLabel(openItem)}</span>
                <span><b class="font-semibold">{strings.detailSize}</b> {formatRisuNestStorageBytes(openItem.byteSize)}</span>
                {#if openItem.structureType === undefined}
                    <span><b class="font-semibold">{strings.detailChars}</b> {openValue.length.toLocaleString()}</span>
                {/if}
            </div>
            {#if openItem.structureType === undefined}
                <pre class="max-h-80 overflow-auto rounded-md border border-darkborderc bg-bgcolor p-3 font-mono text-xs whitespace-pre-wrap">{openValue}</pre>
            {/if}
            <div class="flex justify-end gap-2">
                <SettingButton variant="danger" busy={busy} onclick={() => remove(openItem ? [openItem] : [])}>{language.remove}</SettingButton>
                <SettingButton variant="secondary" onclick={() => { openItem = null }}>{language.risuNest.importDialog.close}</SettingButton>
            </div>
        </div>
    </div>
{/if}

{#if confirming}
    {@const pending = confirming}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/60 p-4">
        <div role="dialog" aria-modal="true" class="flex w-full max-w-md flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="text-lg font-bold">{assignStrings.confirmTitle.replace('{0}', pending.owner)}</h3>
            {#if pending.prefix !== null}
                <p class="text-sm text-subtext">
                    {assignStrings.confirmSummary
                        .replace('{0}', pending.prefix)
                        .replace('{1}', String(pending.items.length))
                        .replace('{2}', formatRisuNestStorageBytes(totalPluginDataBytes(pending.items)))}
                </p>
            {/if}
            <p class="text-sm text-subtext">{assignStrings.confirmBody.replace('{0}', pending.owner)}</p>
            <div class="flex justify-end gap-2">
                <SettingButton variant="secondary" onclick={() => { confirming = null }}>{language.cancel}</SettingButton>
                <SettingButton busy={busy} onclick={confirmAssign}>{assignStrings.confirmAction}</SettingButton>
            </div>
        </div>
    </div>
{/if}

{#if collision}
    {@const pending = collision}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/60 p-4">
        <div role="dialog" aria-modal="true" class="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="text-lg font-bold">{assignStrings.collisionTitle.replace('{0}', String(pending.colliding.length))}</h3>
            <p class="text-sm text-subtext">
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
            <!-- Each answer carries its own consequence, so the text has to wrap. -->
            {#snippet collisionChoice(title: string, help: string, choice: PluginAssignCollision)}
                <button
                    type="button"
                    disabled={busy}
                    class="flex flex-col items-start gap-0.5 rounded-md border border-darkborderc bg-transparent px-3 py-2 text-left text-sm text-textcolor shadow-xs transition-colors duration-200 hover:bg-selected focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected disabled:cursor-not-allowed disabled:opacity-50"
                    onclick={() => applyAssign(pending.owner, pending.items, choice)}
                >
                    <span>{title}</span>
                    <span class="text-xs text-subtext">{help}</span>
                </button>
            {/snippet}
            <div class="flex flex-col gap-2">
                {@render collisionChoice(assignStrings.collisionReplace, assignStrings.collisionReplaceHelp, 'replace')}
                {@render collisionChoice(assignStrings.collisionDiscard, assignStrings.collisionDiscardHelp.replace('{0}', String(pending.colliding.length)), 'discard')}
                {@render collisionChoice(assignStrings.collisionDefer, assignStrings.collisionDeferHelp.replace('{0}', String(pending.colliding.length)), 'defer')}
            </div>
        </div>
    </div>
{/if}

{#if reloadPrompt}
    <div class="fixed inset-0 z-[1200] flex items-center justify-center bg-black/60 p-4">
        <div role="dialog" aria-modal="true" class="flex w-full max-w-md flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor">
            <h3 class="text-lg font-bold">{assignStrings.reloadTitle}</h3>
            <p class="text-sm text-subtext">{assignStrings.reloadBody}</p>
            <div class="flex justify-end gap-2">
                <SettingButton variant="secondary" onclick={() => { reloadPrompt = false }}>{assignStrings.reloadLater}</SettingButton>
                <SettingButton onclick={reloadPlugins}>{assignStrings.reloadNow}</SettingButton>
            </div>
        </div>
    </div>
{/if}
