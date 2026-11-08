<script module lang="ts">
    import { writable } from 'svelte/store'
    // The plugin settings page shows the notice inline, so the root copy stays hidden while it is open.
    const inlineShown = writable(0)
</script>

<script lang="ts">
    import XIcon from '@lucide/svelte/icons/x'
    import { language } from 'src/lang'
    import { DBState, settingsOpen, SettingsMenuIndex } from 'src/ts/stores.svelte'
    import { openRisuNestSettingsTab, activeRisuNestSettingsTab } from 'src/ts/setting/risuNestSettingsTabs'
    import { pluginStartupGate, pluginStartupState, retryPluginStartup } from 'src/ts/plugins/pluginStartupGate'
    import { serverSyncErrorMessage } from 'src/ts/storage/sync/serverSyncErrorMessage'
    import { externalErrorMessage, externalStorageStrings } from '../Setting/ExternalStorage/strings'
    import { syncStatus } from 'src/ts/storage/sync/syncStatus'
    import { syncStatusDisplay } from 'src/ts/storage/sync/syncStatusView'
    import SettingButton from '../Setting/RisuNest/SettingButton.svelte'

    let { inline = false }: { inline?: boolean } = $props()
    let elapsed = $state(false)
    // Closing holds for the current wait, stop or failure; the next one shows again.
    let dismissed = $state<{ phase: string; since: number; stop?: string }>()
    const failed = $derived($pluginStartupState.phase === 'failed')
    const stopped = $derived.by(() => {
        const display = $syncStatus && syncStatusDisplay($syncStatus, externalStorageStrings(DBState.db.language), false)
        return display?.stopped ? display : undefined
    })
    const shown = $derived(!dismissed || dismissed.phase !== $pluginStartupState.phase || dismissed.since !== $pluginStartupState.since
        || (!!stopped && dismissed.stop !== stopped.stopKey))
    // With the sync UI turned off, only a failure or stop that holds plugins is reported.
    const visible = $derived($pluginStartupState.phase !== 'released' && (inline || ($inlineShown === 0 && shown && (failed || !!stopped || (elapsed && (DBState.db.risunestSyncStatusUi ?? true))))))
    const error = $derived.by(() => {
        const value = $pluginStartupState.error
        const unavailable = value instanceof Error && value.message === 'Sync is unavailable'
        const message = stopped?.error ?? ($pluginStartupState.target === 'external'
            ? externalErrorMessage(externalStorageStrings(DBState.db.language), unavailable ? undefined : value)
            : serverSyncErrorMessage(value))
        return failed && message === language.risuNest.serverSync.errorHelp ? '' : message
    })
    $effect(() => {
        const { phase, since } = $pluginStartupState
        elapsed = false
        if (phase === 'released') return
        const timer = setTimeout(() => { elapsed = true }, Math.max(0, since + 1000 - Date.now()))
        return () => clearTimeout(timer)
    })
    $effect(() => {
        if (!inline) return
        inlineShown.update(count => count + 1)
        return () => inlineShown.update(count => count - 1)
    })
    function settings() {
        openRisuNestSettingsTab('sync')
        $SettingsMenuIndex = 17
        $settingsOpen = true
    }
    function dismiss() {
        dismissed = { phase: $pluginStartupState.phase, since: $pluginStartupState.since, stop: stopped?.stopKey }
    }
</script>

{#if visible}
    <aside class="rounded-lg border bg-darkbg p-3 break-keep text-textcolor {inline ? 'mb-2' : 'shadow-lg'} {failed ? 'border-danger-400' : 'border-darkborderc'}" data-plugin-startup>
        <div class="flex items-start gap-2">
            {#if failed || stopped}
                <div class="min-w-0 flex-1">
                    <div role="alert">
                        <p class="font-semibold">{failed ? language.risuNest.pluginStartup.syncUnfinished : language.risuNest.pluginStartup.waiting}</p>
                        {#if stopped}<p class="mt-1 font-semibold">{stopped.label}</p>{/if}
                        {#if error}<p class="mt-1 whitespace-pre-wrap text-sm text-textcolor/70">{error}</p>{/if}
                    </div>
                    {#if !($settingsOpen && $SettingsMenuIndex === 17 && $activeRisuNestSettingsTab === 'sync')}
                        <button type="button" class="-my-2 min-h-11 text-sm text-subtext underline hover:text-textcolor" onclick={settings}>{language.risuNest.tabs.sync}</button>
                    {/if}
                </div>
            {:else}
                <p role="status" class="min-w-0 flex-1">{language.risuNest.pluginStartup.waiting}</p>
            {/if}
            {#if !inline}
                <button type="button" class="-mr-2 -mt-2 flex size-10 shrink-0 items-center justify-center rounded-md text-subtext transition-colors hover:bg-selected hover:text-textcolor" aria-label={language.close} title={language.close} onclick={dismiss}><XIcon size={18} aria-hidden="true" /></button>
            {/if}
        </div>
        {#if failed || stopped}
            <div class="mt-2 flex flex-wrap items-center gap-2">
                <SettingButton onclick={() => void retryPluginStartup()}>{language.retry}</SettingButton>
                <SettingButton variant="secondary" onclick={() => pluginStartupGate.loadOnce()}>{language.risuNest.pluginStartup.loadNow}</SettingButton>
            </div>
        {:else}
            <button type="button" class="-my-2 min-h-11 text-sm underline" onclick={() => pluginStartupGate.loadOnce()}>{language.risuNest.pluginStartup.loadNow}</button>
        {/if}
    </aside>
{/if}
