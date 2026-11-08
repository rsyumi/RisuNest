<script lang="ts">
    import { untrack } from 'svelte'
    import { get } from 'svelte/store'
    import { language } from 'src/lang'
    import { externalErrorMessage, externalStorageStrings } from 'src/lib/Setting/ExternalStorage/strings'
    import { saving } from 'src/ts/globalApi.svelte'
    import { syncStatusPanel } from 'src/ts/gui/syncStatusPanel'
    import { pluginStartupGate, pluginStartupState } from 'src/ts/plugins/pluginStartupGate'
    import { activeRisuNestSettingsTab, openRisuNestSettingsTab } from 'src/ts/setting/risuNestSettingsTabs'
    import { persistentLocalSaveFailure } from 'src/ts/storage/persistentDataRuntime.svelte'
    import { serverSyncErrorMessage } from 'src/ts/storage/sync/serverSyncErrorMessage'
    import { syncStatus } from 'src/ts/storage/sync/syncStatus'
    import { syncStatusDisplay } from 'src/ts/storage/sync/syncStatusView'
    import { DBState, settingsOpen, SettingsMenuIndex, sideBarStore } from 'src/ts/stores.svelte'
    import { backNavigationLayer } from 'src/ts/ui/modalNavigation'
    import SettingButton from '../Setting/RisuNest/SettingButton.svelte'
    import SettingNotice from '../Setting/RisuNest/SettingNotice.svelte'
    import StatusBadge from '../Setting/RisuNest/StatusBadge.svelte'

    const strings = $derived(externalStorageStrings(DBState.db.language))
    const saveFailed = $derived($persistentLocalSaveFailure !== null)
    const display = $derived((DBState.db.risunestSyncStatusUi ?? true) && $syncStatus && syncStatusDisplay($syncStatus, strings, saveFailed))
    let panel = $state<HTMLElement>()
    let place = $state<{ top?: number; bottom?: number; left: number; right?: number; width?: number; height: number }>({ left: 8, height: 0 })
    let pending = $state<number | undefined>()
    let running = $state(false)
    let failure = $state('')

    function close() {
        syncStatusPanel.set(null)
    }
    function measure() {
        const current = get(syncStatusPanel)
        if (!current) return
        const rect = current.anchor.getBoundingClientRect()
        if (current.placement === 'below') {
            const header = current.anchor.closest('[data-mobile-header]')?.getBoundingClientRect() ?? rect
            place = { top: header.bottom + 8, left: 8, right: 8, height: window.innerHeight - header.bottom - 16 }
            return
        }
        // Beside the sidebar column, not the button, so the panel never covers the column.
        const left = (current.anchor.closest('.rs-sidebar') ?? current.anchor).getBoundingClientRect().right + 8
        const width = Math.min(320, window.innerWidth - left - 8)
        // A button in the lower half grows the panel upward from its bottom edge.
        place = rect.top > window.innerHeight / 2
            ? { bottom: window.innerHeight - rect.bottom, left, width, height: rect.bottom - 8 }
            : { top: rect.top, left, width, height: window.innerHeight - rect.top - 8 }
    }
    $effect.pre(() => {
        if (!$syncStatusPanel) return
        untrack(() => {
            measure()
            failure = ''
            const source = get(syncStatus)
            if (source?.target === 'external') source.refresh()
        })
    })
    // The panel belongs to the screen it opened on.
    $effect(() => {
        void $sideBarStore
        void $settingsOpen
        untrack(close)
    })
    $effect(() => {
        if (!display) untrack(close)
    })
    // Read when the panel opens and when a sync attempt ends, as the server card does, not on every progress report.
    const pendingKey = $derived.by(() => {
        const source = $syncStatus
        if (!$syncStatusPanel || source?.target !== 'server') return ''
        const { view } = source
        return `${!!view.status.configured && !!view.status.bound && !view.running && !view.progress}:${view.error}`
    })
    $effect(() => {
        if (!pendingKey) {
            pending = undefined
            return
        }
        const source = untrack(() => get(syncStatus))
        if (source?.target !== 'server') return
        let current = true
        source.pendingChanges().then(value => { if (current) pending = value }, () => { if (current) pending = undefined })
        return () => { current = false }
    })

    function outside(event: PointerEvent) {
        const current = get(syncStatusPanel)
        const target = event.target as Node | null
        if (current && target && !panel?.contains(target) && !current.anchor.contains(target)) close()
    }
    async function syncNow() {
        const source = get(syncStatus)
        if (!source || running) return
        running = true
        failure = ''
        try {
            await source.syncNow()
        } catch (error) {
            failure = source.target === 'server' ? serverSyncErrorMessage(error) : externalErrorMessage(strings, error)
        } finally {
            running = false
        }
    }
    function openSettings() {
        close()
        openRisuNestSettingsTab('sync')
        $SettingsMenuIndex = 17
        $settingsOpen = true
    }
</script>

<svelte:window onpointerdowncapture={outside} onresize={measure} />

{#if $syncStatusPanel && display}
    {@const copy = language.risuNest.serverSync}
    {@const error = failure || display.error}
    <div bind:this={panel} role="dialog" aria-label={language.risuNest.tabs.sync} use:backNavigationLayer={{ close }}
        class="fixed z-[1000] overflow-y-auto rounded-lg border border-darkborderc bg-darkbg p-4 break-keep text-textcolor shadow-lg"
        style:top={place.top === undefined ? null : `${place.top}px`} style:bottom={place.bottom === undefined ? null : `${place.bottom}px`}
        style:left="{place.left}px" style:right={place.right === undefined ? null : `${place.right}px`} style:width={place.width === undefined ? null : `${place.width}px`} style:max-height="{place.height}px"
        data-sync-status-panel>
        <div class="flex items-center justify-between gap-3">
            <h2 class="min-w-0 text-lg font-bold">{display.title}</h2>
            <StatusBadge label={display.label} tone={display.tone} />
        </div>
        <dl class="mt-3 grid grid-cols-[auto_minmax(0,1fr)] gap-x-5 gap-y-1.5 text-[13px] leading-[1.45] [&>dd]:min-w-0 [&>dd]:tabular-nums [&>dt]:whitespace-nowrap [&>dt]:text-subtext">
            {#if display.lastSuccessAt !== undefined}<dt>{copy.lastSuccess}</dt><dd>{new Date(display.lastSuccessAt).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' })}</dd>{/if}
            {#if pending !== undefined}<dt>{copy.pendingChanges}</dt><dd>{copy.count.replace('{0}', pending.toLocaleString())}</dd>{/if}
            <dt>{strings.thisDevice}</dt>
            <dd>{saveFailed ? language.risuNest.syncStatus.saveFailed : saving.state ? language.risuNest.syncStatus.saving : language.risuNest.syncStatus.saved}</dd>
        </dl>
        {#if error}<div class="mt-3"><SettingNotice text={error} tone={!failure && display.tone === 'failed' ? 'info' : 'danger'} /></div>{/if}
        {#if $pluginStartupState.phase !== 'released'}
            <div class="mt-3 border-t border-darkborderc pt-3 text-sm">
                <p>{$pluginStartupState.phase === 'failed' ? language.risuNest.pluginStartup.syncUnfinished : language.risuNest.pluginStartup.waiting}</p>
                <button type="button" class="-my-2 min-h-11 underline" onclick={() => pluginStartupGate.loadOnce()}>{language.risuNest.pluginStartup.loadNow}</button>
            </div>
        {/if}
        <div class="mt-3 flex items-center gap-2 border-t border-darkborderc pt-3">
            <SettingButton busy={running} onclick={syncNow}>{copy.syncNow}</SettingButton>
            {#if !($settingsOpen && $SettingsMenuIndex === 17 && $activeRisuNestSettingsTab === 'sync')}
                <button type="button" class="ml-auto min-h-11 text-sm text-subtext underline hover:text-textcolor" onclick={openSettings}>{language.risuNest.tabs.sync}</button>
            {/if}
        </div>
    </div>
{/if}
