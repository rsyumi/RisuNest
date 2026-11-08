<script lang="ts">
    import { get } from 'svelte/store'
    import { language } from 'src/lang'
    import { risuNestInlaySettingsItems, risuNestStreamingSettingsItems, risuNestUiSettingsItems } from 'src/ts/setting/risuNestSettingsData'
    import {
        RISUNEST_SETTINGS_TABS,
        risuNestSettingsTabRequest,
        activeRisuNestSettingsTab,
        type RisuNestSettingsTab,
    } from 'src/ts/setting/risuNestSettingsTabs'
    import { isTauri, isTauriAndroid, isTauriIOS } from 'src/ts/platform'
    import { DBState } from 'src/ts/stores.svelte'
    import RisuNestSettingRows from '../RisuNest/RisuNestSettingRows.svelte'
    import RisuNestPerformanceSettings from './RisuNestPerformanceSettings.svelte'
    import RisuNestAttachmentGallery from './RisuNestAttachmentGallery.svelte'
    import { galleryText } from 'src/ts/gallery/strings'
    import RisuNestInlayInventory from './RisuNestInlayInventory.svelte'
    import RisuNestImageGeometry from './RisuNestImageGeometry.svelte'
    import RisuNestPluginData from './RisuNestPluginData.svelte'
    import RisuNestLocalData from './RisuNestLocalData.svelte'
    import RisuNestPluginStartup from './RisuNestPluginStartup.svelte'
    import ServerSyncSettings from './ServerSyncSettings.svelte'
    import RisuNestStorageDashboard from './RisuNestStorageDashboard.svelte'
    import RisuNestDataHealth from './RisuNestDataHealth.svelte'
    import RisuNestBackupRestore from './RisuNestBackupRestore.svelte'
    import RisuNestAppImage from './RisuNestAppImage.svelte'
    import RisuNestIOSPlatform from './RisuNestIOSPlatform.svelte'
    import RisuNestAndroidPlatform from './RisuNestAndroidPlatform.svelte'
    import RisuNestNetworkLog from './RisuNestNetworkLog.svelte'
    import RisuNestLogViewer from './RisuNestLogViewer.svelte'
    import RisuNestUpdateSettings from './RisuNestUpdateSettings.svelte'
    import RisuNestAppLockSettings from './RisuNestAppLockSettings.svelte'
    import ExternalStorageSettings from '../ExternalStorage/ExternalStorageSettings.svelte'
    import LocalDataReset from '../RisuNest/LocalDataReset.svelte'
    import RisuNestStartupExclusions from './RisuNestStartupExclusions.svelte'

    const tabLabels: Record<RisuNestSettingsTab, string> = {
        settings: language.risuNest.tabs.settings,
        storage: language.risuNest.tabs.storage,
        attachments: galleryText().attachmentTab,
        sync: language.risuNest.tabs.sync,
        'plugin-data': language.risuNest.tabs.pluginData,
        'network-logs': language.risuNest.tabs.networkLogs,
    }

    const visibleTabs = RISUNEST_SETTINGS_TABS.filter(tab => isTauri || tab !== 'network-logs')

    // A screen that sends the reader here names the tab; otherwise the page opens on the first one.
    let activeTab: RisuNestSettingsTab = $state(get(risuNestSettingsTabRequest) ?? 'settings')
    let tabButtons: HTMLButtonElement[] = $state([])
    $effect(() => {
        activeRisuNestSettingsTab.set(activeTab)
        return () => activeRisuNestSettingsTab.set(null)
    })

    $effect(() => {
        const requested = $risuNestSettingsTabRequest
        if (!requested) return
        activeTab = visibleTabs.includes(requested) ? requested : 'settings'
        risuNestSettingsTabRequest.set(null)
    })

    function jumpTo(id: string): void {
        document.getElementById(id)?.scrollIntoView({ block: 'start', behavior: 'smooth' })
    }

    function moveTab(event: KeyboardEvent, index: number): void {
        const last = visibleTabs.length - 1
        let next: number
        if (event.key === 'ArrowRight') next = index === last ? 0 : index + 1
        else if (event.key === 'ArrowLeft') next = index === 0 ? last : index - 1
        else if (event.key === 'Home') next = 0
        else if (event.key === 'End') next = last
        else return
        event.preventDefault()
        activeTab = visibleTabs[next]
        tabButtons[next]?.focus()
    }
</script>

<!-- Korean has no spaces inside a word, so the default break rule splits words mid-syllable. -->
<div class="@container w-full" class:max-w-3xl={activeTab !== 'attachments'} class:break-keep={DBState.db.language === 'ko'}>
    <h1 class="text-2xl font-bold">{language.risuNest.menuTitle}</h1>
    <div
        role="tablist"
        aria-label={language.risuNest.tabList}
        class="-mx-4 mt-3 flex overflow-x-auto border-b border-darkborderc px-4 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden @xl:mx-0 @xl:px-0"
    >
        {#each visibleTabs as tab, index (tab)}
            {@const active = activeTab === tab}
            <button
                bind:this={tabButtons[index]}
                type="button"
                role="tab"
                id="risunest-tab-{tab}"
                data-risunest-tab={tab}
                aria-selected={active}
                aria-controls="risunest-panel-{tab}"
                tabindex={active ? 0 : -1}
                class="-mb-px shrink-0 border-b-2 px-3 py-2 text-sm whitespace-nowrap transition-colors duration-200 focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected {active ? 'border-textcolor font-semibold text-textcolor' : 'border-transparent text-subtext hover:text-textcolor'}"
                onclick={() => { activeTab = tab }}
                onkeydown={(event) => moveTab(event, index)}
            >{tabLabels[tab]}</button>
        {/each}
    </div>
    <div role="tabpanel" id="risunest-panel-{activeTab}" aria-labelledby="risunest-tab-{activeTab}" data-risunest-panel={activeTab}>
        {#if activeTab === 'settings'}
            <RisuNestStartupExclusions />
            <RisuNestPerformanceSettings />
            <RisuNestSettingRows items={risuNestUiSettingsItems} />
            <RisuNestSettingRows items={risuNestStreamingSettingsItems} />
            <RisuNestSettingRows items={risuNestInlaySettingsItems} />
            {#if isTauri}
                <RisuNestAppLockSettings />
                <RisuNestUpdateSettings />
            {/if}
            {#if isTauri && !isTauriAndroid && !isTauriIOS}
                <RisuNestAppImage />
            {/if}
            {#if isTauriIOS}
                <RisuNestIOSPlatform />
            {/if}
            {#if isTauriAndroid}
                <RisuNestAndroidPlatform />
            {/if}
            {#if isTauri}
                <RisuNestLogViewer />
            {/if}
        {:else if activeTab === 'storage'}
            {#if isTauri}
                <RisuNestStorageDashboard />
            {/if}
            <RisuNestInlayInventory />
            {#if isTauri}
                <RisuNestImageGeometry />
                <RisuNestDataHealth onOpenUnusedImages={() => jumpTo('risunest-storage')} />
            {/if}
            <RisuNestBackupRestore />
            <LocalDataReset />
        {:else if activeTab === 'attachments'}
            <RisuNestAttachmentGallery />
        {:else if activeTab === 'sync'}
            {#if isTauri}
                <ServerSyncSettings />
            {/if}
            <ExternalStorageSettings />
            {#if isTauri}
                <RisuNestPluginStartup />
                <RisuNestLocalData />
            {/if}
        {:else if activeTab === 'network-logs' && isTauri}
            <RisuNestNetworkLog />
        {:else}
            <RisuNestPluginData />
        {/if}
    </div>
</div>
