<script lang="ts">
    import { get } from 'svelte/store'
    import { language } from 'src/lang'
    import { risuNestInlaySettingsItems, risuNestStreamingSettingsItems } from 'src/ts/setting/risuNestSettingsData'
    import {
        RISUNEST_SETTINGS_TABS,
        risuNestSettingsTabRequest,
        type RisuNestSettingsTab,
    } from 'src/ts/setting/risuNestSettingsTabs'
    import { isTauri, isTauriAndroid, isTauriIOS } from 'src/ts/platform'
    import RisuNestSettingRows from '../RisuNest/RisuNestSettingRows.svelte'
    import RisuNestPerformanceSettings from './RisuNestPerformanceSettings.svelte'
    import RisuNestInlayInventory from './RisuNestInlayInventory.svelte'
    import RisuNestPluginData from './RisuNestPluginData.svelte'
    import RisuNestLocalData from './RisuNestLocalData.svelte'
    import RisuNestStorageDashboard from './RisuNestStorageDashboard.svelte'
    import RisuNestDataHealth from './RisuNestDataHealth.svelte'
    import RisuNestBackupRestore from './RisuNestBackupRestore.svelte'
    import RisuNestAppImage from './RisuNestAppImage.svelte'
    import RisuNestIOSPlatform from './RisuNestIOSPlatform.svelte'
    import RisuNestAndroidPlatform from './RisuNestAndroidPlatform.svelte'
    import RisuNestLogViewer from './RisuNestLogViewer.svelte'
    import ServerSyncSettings from './ServerSyncSettings.svelte'
    import RisuNestUpdateSettings from './RisuNestUpdateSettings.svelte'
    import ExternalStorageSettings from '../ExternalStorage/ExternalStorageSettings.svelte'

    const tabLabels: Record<RisuNestSettingsTab, string> = {
        settings: language.risuNest.tabs.settings,
        storage: language.risuNest.tabs.storage,
        sync: language.risuNest.tabs.sync,
        'plugin-data': language.risuNest.tabs.pluginData,
    }

    // A screen that sends the reader here names the tab; otherwise the page opens on the first one.
    let activeTab: RisuNestSettingsTab = $state(get(risuNestSettingsTabRequest) ?? 'settings')
    let tabButtons: HTMLButtonElement[] = $state([])

    $effect(() => {
        const requested = $risuNestSettingsTabRequest
        if (!requested) return
        activeTab = requested
        risuNestSettingsTabRequest.set(null)
    })

    function jumpTo(id: string): void {
        document.getElementById(id)?.scrollIntoView({ block: 'start', behavior: 'smooth' })
    }

    function moveTab(event: KeyboardEvent, index: number): void {
        const last = RISUNEST_SETTINGS_TABS.length - 1
        let next: number
        if (event.key === 'ArrowRight') next = index === last ? 0 : index + 1
        else if (event.key === 'ArrowLeft') next = index === 0 ? last : index - 1
        else if (event.key === 'Home') next = 0
        else if (event.key === 'End') next = last
        else return
        event.preventDefault()
        activeTab = RISUNEST_SETTINGS_TABS[next]
        tabButtons[next]?.focus()
    }
</script>

<div class="@container w-full max-w-3xl">
    <h1 class="text-2xl font-bold">{language.risuNest.menuTitle}</h1>
    <div
        role="tablist"
        aria-label={language.risuNest.tabList}
        class="-mx-4 mt-3 flex overflow-x-auto border-b border-darkborderc px-4 [scrollbar-width:none] @xl:mx-0 @xl:px-0"
    >
        {#each RISUNEST_SETTINGS_TABS as tab, index (tab)}
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
                class="-mb-px shrink-0 border-b-2 px-3 py-2 text-sm whitespace-nowrap transition-colors duration-200 focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected {active ? 'border-textcolor font-semibold text-textcolor' : 'border-transparent text-textcolor2 hover:text-textcolor'}"
                onclick={() => { activeTab = tab }}
                onkeydown={(event) => moveTab(event, index)}
            >{tabLabels[tab]}</button>
        {/each}
    </div>
    <div role="tabpanel" id="risunest-panel-{activeTab}" aria-labelledby="risunest-tab-{activeTab}" data-risunest-panel={activeTab}>
        {#if activeTab === 'settings'}
            <RisuNestPerformanceSettings />
            <RisuNestSettingRows items={risuNestStreamingSettingsItems} />
            <RisuNestSettingRows items={risuNestInlaySettingsItems} />
            {#if isTauri}
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
                <RisuNestDataHealth onOpenUnusedImages={() => jumpTo('risunest-storage')} />
            {/if}
            <RisuNestBackupRestore />
        {:else if activeTab === 'sync'}
            {#if isTauri}
                <section id="risunest-server-sync" class="scroll-mt-4"><ServerSyncSettings /></section>
            {/if}
            <ExternalStorageSettings />
            {#if isTauri}
                <RisuNestLocalData />
            {/if}
        {:else}
            <RisuNestPluginData />
        {/if}
    </div>
</div>
