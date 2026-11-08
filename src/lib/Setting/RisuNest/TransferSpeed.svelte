<script lang="ts">
    import { onMount } from 'svelte'
    import ArrowUpFromLineIcon from '@lucide/svelte/icons/arrow-up-from-line'
    import ArrowDownToLineIcon from '@lucide/svelte/icons/arrow-down-to-line'
    import { createRateMeter, type TransferRateSample } from 'src/ts/storage/sync/transferRate'
    import { formatRisuNestStorageBytes as bytes } from 'src/ts/storage/risuNestStorageDashboard'

    let { sample, active = true, uploadLabel, downloadLabel }: { sample: TransferRateSample; active?: boolean; uploadLabel: string; downloadLabel: string } = $props()
    const upload = createRateMeter(), download = createRateMeter()
    let id: string | undefined, at = -1, receivedAt = 0
    let rates = $state<{ upload?: number; download?: number }>({})
    function refresh() {
        const now = at + performance.now() - receivedAt
        rates = { upload: upload.rate(now), download: download.rate(now) }
    }
    $effect(() => {
        if (sample.id !== id) { id = sample.id; at = -1; upload.reset(); download.reset() }
        if (sample.atMs > at) {
            at = sample.atMs; receivedAt = performance.now()
            upload.add(at, Number(sample.sentBytes)); download.add(at, Number(sample.receivedBytes))
        }
        refresh()
    })
    onMount(() => { const timer = setInterval(refresh, 500); return () => clearInterval(timer) })
    // Request bodies leave in short bursts, so a direction stays while its average over the meter window is above zero.
    const shown = (moving: boolean, rate: number | undefined) => active && rate !== undefined && (moving || rate > 0)
    let uploading = $derived(shown(sample.sending, rates.upload))
    let downloading = $derived(shown(sample.receiving, rates.download))
</script>

{#if uploading || downloading}
    <span class="speed" data-transfer-speed>
        {#if uploading}<span class="rate"><ArrowUpFromLineIcon size={13} class="shrink-0" aria-hidden="true" /><span class="sr-only">{uploadLabel}</span> {bytes(rates.upload!)}/s</span>{/if}
        {#if downloading}<span class="rate"><ArrowDownToLineIcon size={13} class="shrink-0" aria-hidden="true" /><span class="sr-only">{downloadLabel}</span> {bytes(rates.download!)}/s</span>{/if}
    </span>
{/if}

<style>
    .speed { display: inline-flex; flex-wrap: wrap; justify-content: flex-end; gap: 0 0.75rem; margin-left: auto; color: var(--color-subtext); font-size: 12px; line-height: 18px; font-variant-numeric: tabular-nums; white-space: nowrap; }
    .rate { display: inline-flex; align-items: center; gap: 3px; }
</style>
