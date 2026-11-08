<script lang="ts">
    import { language } from 'src/lang'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
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
            // An operation that settled as not applied closes the dialog before its error arrives.
            refreshFailed = open
        } finally {
            refreshing = false
        }
    }
</script>

{#if open}
    <div class="fixed inset-0 z-work-dialog-recovery flex items-center justify-center bg-black/60 p-4">
        <div
            bind:this={panel}
            tabindex="-1"
            role="dialog"
            aria-modal="true"
            aria-label={title}
            data-testid="persistent-working-set-recovery"
            class="flex max-h-[90dvh] w-full max-w-lg flex-col gap-4 overflow-y-auto rounded-lg border border-darkborderc bg-darkbg p-5 text-textcolor outline-hidden">
            <h2 class="text-lg font-bold">{title}</h2>
            <p class="text-sm text-subtext">{unknown
                ? language.risuNest.persistentData.confirmApplicationHelp
                : language.risuNest.persistentData.refreshHelp}</p>
            {#if refreshFailed}
                <p role="alert" class="text-sm text-danger-400">{unknown
                    ? language.risuNest.persistentData.confirmApplicationFailed
                    : language.risuNest.persistentData.refreshFailed}</p>
            {/if}
            <SettingButton class="self-end" busy={refreshing} onclick={refresh}>
                {unknown
                    ? language.risuNest.persistentData.confirmApplication
                    : language.risuNest.persistentData.refresh}
            </SettingButton>
        </div>
    </div>
{/if}
