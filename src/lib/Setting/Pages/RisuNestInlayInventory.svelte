<script lang="ts">
    import { language } from 'src/lang'
    import { isTauri } from 'src/ts/platform'
    import { alertConfirm } from 'src/ts/alert'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingProgress from '../RisuNest/SettingProgress.svelte'
    import { getInlayEncodeOptions, listInlayAssetMetadata } from 'src/ts/process/files/inlays'
    import {
        summarizeInlayAssets,
        type InlayInventory,
        type InlayInventoryEntry,
    } from 'src/ts/process/files/inlayInventory'
    import {
        emptyInlayOptimizationProgress,
        runInlayOptimization,
        selectInlayOptimizationTargets,
        type InlayOptimizationProgress,
    } from 'src/ts/process/files/inlayOptimizationJob'
    import {
        inlayOptimizationConfirmMessage,
        inlayOptimizationProgressMessage,
        inlayOptimizationResultMessage,
    } from 'src/ts/process/files/inlayOptimizationMessages'
    import {
        createStoredInlayOptimizationDeps,
        readInlayOptimizationEnvironment,
    } from 'src/ts/process/files/inlayOptimizationRuntime'
    import { formatRisuNestStorageBytes } from 'src/ts/storage/risuNestStorageDashboard'
    import type { InlayBlobMetadata } from 'src/ts/storage/blobStore'

    const strings = language.risuNest.inlay
    let assets: InlayBlobMetadata[] | null = $state(null)
    let loading = $state(false)
    let loadFailed = $state(false)
    let optimizing = $state(false)
    let cancelRequested = $state(false)
    let progress: InlayOptimizationProgress = $state(emptyInlayOptimizationProgress())
    let resultText = $state('')
    let inventory: InlayInventory | null = $derived(assets ? summarizeInlayAssets(assets) : null)
    let summary = $derived(
        inventory
            ? strings.inventoryTotal
                  .replace('{count}', inventory.total.count.toLocaleString())
                  .replace('{size}', formatRisuNestStorageBytes(inventory.total.bytes))
            : '',
    )
    let progressText = $derived(inlayOptimizationProgressMessage(progress))

    async function load(): Promise<void> {
        if (loading) return
        loading = true
        loadFailed = false
        try {
            assets = await listInlayAssetMetadata()
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

    async function optimize(): Promise<void> {
        if (!assets || optimizing) return
        const options = getInlayEncodeOptions()
        const targets = selectInlayOptimizationTargets(assets, options)
        resultText = ''
        if (targets.length === 0) {
            resultText = strings.optimizeNone
            return
        }
        const message = inlayOptimizationConfirmMessage({
            targets,
            storedFormat: options.format,
            ...await readInlayOptimizationEnvironment(),
        })
        if (!await alertConfirm(message)) return
        optimizing = true
        cancelRequested = false
        progress = emptyInlayOptimizationProgress(targets.length)
        try {
            const done = await runInlayOptimization(targets, createStoredInlayOptimizationDeps(), {
                options,
                isCancelled: () => cancelRequested,
                onProgress: (value) => { progress = value },
            })
            resultText = inlayOptimizationResultMessage(done)
        } finally {
            optimizing = false
            cancelRequested = false
        }
        await load()
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

<SettingGroup
    id="risunest-inlay-inventory"
    title={strings.inventoryTitle}
    description={isTauri ? undefined : strings.animationWebOnly}
    divide={false}
>
    {#snippet actions()}
        {#if inventory}
            <SettingButton variant="secondary" busy={loading} disabled={optimizing} onclick={load}>{strings.inventoryRefresh}</SettingButton>
        {/if}
    {/snippet}
    {#if loadFailed}
        <div class="px-4 py-3 text-sm text-textcolor2" role="alert" aria-live="assertive">{strings.inventoryLoadFailed}</div>
    {/if}
    {#if !inventory}
        <div class="flex justify-center px-4 py-4">
            <SettingButton busy={loading} onclick={load}>{strings.inventoryLoad}</SettingButton>
        </div>
    {:else if inventory.total.count === 0}
        <div class="px-4 py-3 text-sm text-textcolor2">{strings.inventoryEmpty}</div>
    {:else}
        <div data-inlay-inventory-summary class="px-4 pt-4 pb-1 text-[1.4rem] leading-tight font-bold tabular-nums">{summary}</div>
        {@render table(strings.inventoryImages, inventory.images)}
        {#if inventory.others.length > 0}
            {@render table(strings.inventoryOthers, inventory.others)}
        {/if}
        {#if inventory.images.length > 0}
            <div class="border-t border-darkborderc/55">
                <SettingRow label={strings.optimize} help={strings.optimizeHelp}>
                    {#snippet below()}
                        {#if optimizing}
                            <div data-inlay-optimize-progress class="mt-2">
                                <SettingProgress
                                    label={strings.optimize}
                                    detail={progressText}
                                    fraction={progress.total > 0 ? progress.scanned / progress.total : null}
                                />
                            </div>
                        {:else if resultText}
                            <p data-inlay-optimize-result class="mt-1 text-sm text-textcolor2" role="status" aria-live="polite">{resultText}</p>
                        {/if}
                    {/snippet}
                    {#if optimizing}
                        <SettingButton variant="secondary" disabled={cancelRequested} onclick={() => { cancelRequested = true }}>{language.cancel}</SettingButton>
                    {:else}
                        <SettingButton disabled={loading} onclick={optimize}>{strings.optimize}</SettingButton>
                    {/if}
                </SettingRow>
            </div>
        {/if}
    {/if}
</SettingGroup>
