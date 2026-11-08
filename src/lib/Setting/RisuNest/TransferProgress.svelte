<script lang="ts">
    import type { Snippet } from 'svelte'
    import { CheckIcon, ChevronRightIcon, LoaderCircleIcon } from '@lucide/svelte'
    import SettingProgress from './SettingProgress.svelte'
    import TransferSpeed from './TransferSpeed.svelte'
    import type { TransferRateSample } from 'src/ts/storage/sync/transferRate'

    let { label, detail = '', fraction = null, done = false, stopped = false, activity = '',
        stages = [], counters = [], detailsLabel, collapsible = false, open = $bindable(false), actions, speed }: {
        label: string
        detail?: string
        fraction?: number | null
        done?: boolean
        stopped?: boolean
        activity?: string
        stages?: Array<{ label: string; state: 'active' | 'done' }>
        counters?: Array<{ key: string; label: string; value: string }>
        detailsLabel: string
        collapsible?: boolean
        open?: boolean
        actions?: Snippet
        /** The network samples of a transfer. Its rates follow the detail while `active`. */
        speed?: { sample: TransferRateSample; active: boolean; uploadLabel: string; downloadLabel: string }
    } = $props()
</script>

{#snippet rate()}
    {#if speed}<TransferSpeed sample={speed.sample} active={speed.active && !stopped} uploadLabel={speed.uploadLabel} downloadLabel={speed.downloadLabel} />{/if}
{/snippet}

{#snippet information()}
    {#if activity}<p class="activity">{activity}</p>{/if}
    {#if stages.length > 1}
        <ol class="stages">
            {#each stages as stage}
                <li data-state={stage.state} aria-current={stage.state === 'active' ? 'step' : undefined}>
                    {#if stage.state === 'done'}<CheckIcon size={14} aria-hidden="true" />
                    {:else if !stopped}<LoaderCircleIcon size={14} class="motion-safe:animate-spin" aria-hidden="true" />{/if}
                    <span>{stage.label}</span>
                </li>
            {/each}
        </ol>
    {/if}
    {#if counters.length}
        <dl class="counters">
            {#each counters as counter (counter.key)}<dt>{counter.label}</dt><dd>{counter.value}</dd>{/each}
        </dl>
    {/if}
{/snippet}

<div class="transfer-progress">
    <SettingProgress {label} {detail} {fraction} {done} {stopped} {actions} trailing={speed && !done ? rate : undefined} />
    {#if activity || stages.length > 1 || counters.length}
        {#if collapsible}
            <details class="group" bind:open>
                <summary class="flex cursor-pointer list-none items-center gap-2 text-sm text-subtext select-none [&::-webkit-details-marker]:hidden">
                    <ChevronRightIcon size={16} class="shrink-0 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" />
                    <span>{detailsLabel}</span>
                </summary>
                <div class="details">{@render information()}</div>
            </details>
        {:else}
            {@render information()}
        {/if}
    {/if}
</div>

<style>
    .transfer-progress, .details { display: grid; gap: 0.625rem; min-width: 0; }
    .details { margin-top: 0.5rem; padding-left: 1.5rem; }
    .activity, .stages, .counters { font-size: 13px; }
    .stages { display: grid; gap: 0.375rem; margin: 0; padding: 0; list-style: none; }
    .stages li { display: flex; align-items: center; gap: 0.5rem; min-width: 0; color: var(--color-subtext); }
    .stages li[data-state='active'] { color: var(--risu-theme-textcolor); }
    .stages li[data-state='done'] :global(svg) { color: var(--risu-theme-success-500); }
    .counters { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 0.375rem 1rem; margin: 0; }
    .counters dt { color: var(--color-subtext); }
    .counters dd { margin: 0; text-align: right; font-variant-numeric: tabular-nums; overflow-wrap: anywhere; }
</style>
