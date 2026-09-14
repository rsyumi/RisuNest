<script lang="ts">
    import { language } from 'src/lang'
    import { isTauri } from 'src/ts/platform'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import { listInlayAssetMetadata } from 'src/ts/process/files/inlays'
    import {
        summarizeInlayAssets,
        type InlayInventory,
        type InlayInventoryEntry,
    } from 'src/ts/process/files/inlayInventory'
    import { formatRisuNestStorageBytes } from 'src/ts/storage/risuNestStorageDashboard'

    const strings = language.risuNest.inlay
    let inventory: InlayInventory | null = $state(null)
    let loading = $state(false)
    let loadFailed = $state(false)
    let summary = $derived(
        inventory
            ? strings.inventoryTotal
                  .replace('{count}', inventory.total.count.toLocaleString())
                  .replace('{size}', formatRisuNestStorageBytes(inventory.total.bytes))
            : '',
    )

    async function load(): Promise<void> {
        if (loading) return
        loading = true
        loadFailed = false
        try {
            inventory = summarizeInlayAssets(await listInlayAssetMetadata({ migrateLegacy: !isTauri }))
        } catch (error) {
            void error
            loadFailed = true
        } finally {
            loading = false
        }
    }

    function share(bytes: number): string {
        return `${(bytes / Math.max(1, inventory?.total.bytes ?? 0)) * 100}%`
    }
</script>

{#snippet table(caption: string, rows: InlayInventoryEntry[])}
    <table data-inlay-inventory-table class="w-full border-t border-darkborderc/55 text-sm">
        <caption class="px-4 pt-3 pb-1 text-left text-xs text-textcolor2">{caption}</caption>
        <thead>
            <tr class="text-xs text-textcolor2">
                <th scope="col" class="px-4 py-1 text-left font-normal">{strings.inventoryExtension}</th>
                <th aria-hidden="true" class="hidden w-[35%] @md:table-cell"></th>
                <th scope="col" class="px-4 py-1 text-right font-normal">{strings.inventoryCount}</th>
                <th scope="col" class="px-4 py-1 text-right font-normal">{strings.inventorySize}</th>
            </tr>
        </thead>
        <tbody>
            {#each rows as row (row.ext)}
                <tr data-inlay-inventory-row>
                    <td class="px-4 py-1.5 break-all">{row.ext || strings.inventoryNoExtension}</td>
                    <td aria-hidden="true" class="hidden py-1.5 @md:table-cell">
                        <div class="h-2 overflow-hidden rounded-full bg-bgcolor">
                            <div class="h-full bg-borderc" style:width={share(row.bytes)}></div>
                        </div>
                    </td>
                    <td class="px-4 py-1.5 text-right tabular-nums">{row.count.toLocaleString()}</td>
                    <td class="px-4 py-1.5 text-right tabular-nums">{formatRisuNestStorageBytes(row.bytes)}</td>
                </tr>
            {/each}
        </tbody>
    </table>
{/snippet}

<SettingGroup id="risunest-inlay-inventory" title={strings.inventoryTitle} divide={false}>
    {#snippet actions()}
        {#if inventory}
            <Button size="sm" styled="outlined" disabled={loading} onclick={load}>{loading ? language.loading : strings.inventoryRefresh}</Button>
        {/if}
    {/snippet}
    {#if loadFailed}
        <div class="px-4 py-3 text-sm text-textcolor2" role="alert" aria-live="assertive">{strings.inventoryLoadFailed}</div>
    {/if}
    {#if !inventory}
        <div class="flex justify-center px-4 py-4">
            <Button size="sm" disabled={loading} onclick={load}>{loading ? language.loading : strings.inventoryLoad}</Button>
        </div>
    {:else if inventory.total.count === 0}
        <div class="px-4 py-3 text-sm text-textcolor2">{strings.inventoryEmpty}</div>
    {:else}
        <div data-inlay-inventory-summary class="px-4 pt-4 pb-1 text-[1.4rem] leading-tight font-bold tabular-nums">{summary}</div>
        {@render table(strings.inventoryImages, inventory.images)}
        {#if inventory.others.length > 0}
            {@render table(strings.inventoryOthers, inventory.others)}
        {/if}
    {/if}
</SettingGroup>
