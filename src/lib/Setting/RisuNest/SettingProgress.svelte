<script lang="ts">
    import type { Snippet } from 'svelte'
    import { CheckIcon, LoaderCircleIcon } from '@lucide/svelte'

    interface Props {
        /** What is running, in the user's words. */
        label: string
        /** Counts or bytes the operation reports, shown beside the label. */
        detail?: string
        /** Share done between 0 and 1. Omit it while the operation reports no total. */
        fraction?: number | null
        /** The operation finished: a check replaces the spinner. */
        done?: boolean
        /** The operation ended without finishing: the spinner and the pulse stop. */
        stopped?: boolean
        /** Controls for the running operation, such as a cancel button. */
        actions?: Snippet
        /**
         * Figures that change often, such as transfer speed. They follow the detail on the
         * heading row, outside the live region so their updates are not announced.
         */
        trailing?: Snippet
    }

    let { label, detail, fraction = null, done = false, stopped = false, actions, trailing }: Props = $props()

    let percent = $derived(fraction === null ? null : Math.max(0, Math.min(100, Math.round(fraction * 100))))
</script>

<div data-setting-progress class="flex flex-col gap-2 rounded-md border border-darkborderc bg-bgcolor px-3 py-2.5">
    <div class="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <div role="status" aria-live="polite" class="flex min-w-40 flex-[1_1_0%] flex-wrap items-center gap-x-3 gap-y-1.5">
            <span class="flex min-w-0 flex-auto items-center gap-3">
                {#if done}
                    <CheckIcon size={15} class="shrink-0 text-success-500" aria-hidden="true" />
                {:else if !stopped}
                    <LoaderCircleIcon size={15} class="shrink-0 text-borderc motion-safe:animate-spin" aria-hidden="true" />
                {/if}
                <span class="min-w-0 flex-auto text-sm">{label}</span>
            </span>
            {#if detail}
                <span class="text-xs text-subtext tabular-nums">{detail}</span>
            {/if}
            {#if percent !== null}
                <span class="text-xs text-subtext tabular-nums">{percent}%</span>
            {/if}
        </div>
        {#if trailing}{@render trailing()}{/if}
        {#if actions}
            <div class="flex items-center gap-2">{@render actions()}</div>
        {/if}
    </div>
    <div
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent ?? undefined}
        class="h-1.5 w-full overflow-hidden rounded-full bg-darkbutton"
    >
        {#if percent === null}
            <div class="h-full w-full bg-borderc/60 {done || stopped ? '' : 'motion-safe:animate-pulse'}"></div>
        {:else}
            <div class="h-full bg-borderc transition-[width] duration-300" style:width={`${percent}%`}></div>
        {/if}
    </div>
</div>
