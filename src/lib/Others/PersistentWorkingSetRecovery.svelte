<script lang="ts">
    import { language } from 'src/lang'
    import {
        persistentWorkingSetRefreshRevision,
        retryCommittedWorkingSetRefresh,
    } from 'src/ts/storage/persistentDataRuntime.svelte'

    import {
        externalApplicationRecovery,
        retryExternalApplication,
    } from 'src/ts/storage/sync/external/applicationRecovery'

    const unknown = $derived($externalApplicationRecovery?.confirmationPending === true)
    const open = $derived($externalApplicationRecovery !== null || $persistentWorkingSetRefreshRevision !== null)
    const title = $derived(unknown
        ? language.risuNest.persistentData.confirmApplicationTitle
        : language.risuNest.persistentData.refreshTitle)
    let panel = $state<HTMLDivElement | undefined>()
    let refreshing = $state(false)
    let refreshFailed = $state(false)

    $effect(() => {
        if (open) panel?.focus()
        else refreshFailed = false
    })

    async function refresh(): Promise<void> {
        if (refreshing) return
        refreshing = true
        refreshFailed = false
        try {
            if ($externalApplicationRecovery !== null) {
                await retryExternalApplication()
            } else {
                const outcome = await retryCommittedWorkingSetRefresh()
                refreshFailed = outcome?.projection === 'refresh-required'
            }
        } catch {
            refreshFailed = true
        } finally {
            refreshing = false
        }
    }
</script>

{#if open}
    <div class="fixed inset-0 z-[1100] flex items-center justify-center bg-darkbg/70 p-4">
        <div
            bind:this={panel}
            tabindex="-1"
            role="dialog"
            aria-modal="true"
            aria-label={title}
            data-testid="persistent-working-set-recovery"
            class="flex w-full max-w-lg flex-col gap-4 rounded-lg border border-darkborderc bg-darkbg p-5 text-textcolor outline-hidden">
                <h2 class="text-lg font-bold">{title}</h2>
                <p class="text-sm text-textcolor2">{unknown
                    ? language.risuNest.persistentData.confirmApplicationHelp
                    : language.risuNest.persistentData.refreshHelp}</p>
                {#if refreshFailed}
                    <p role="alert" class="text-sm text-textcolor2">{unknown
                        ? language.risuNest.persistentData.confirmApplicationFailed
                        : language.risuNest.persistentData.refreshFailed}</p>
                {/if}
                <button
                    type="button"
                    disabled={refreshing}
                    class="self-end rounded-md border border-darkborderc bg-darkbutton px-4 py-2 hover:bg-selected disabled:opacity-50"
                    onclick={refresh}>
                    {refreshing ? language.loading : unknown
                        ? language.risuNest.persistentData.confirmApplication
                        : language.risuNest.persistentData.refresh}
                </button>
        </div>
    </div>
{/if}
