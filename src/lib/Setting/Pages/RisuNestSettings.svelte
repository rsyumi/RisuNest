<script lang="ts">
    import { language } from 'src/lang'
    import { risuNestSettingsItems } from 'src/ts/setting/risuNestSettingsData'
    import { isTauri, isTauriAndroid, isTauriIOS } from 'src/ts/platform'
    import RisuNestSettingRows from '../RisuNest/RisuNestSettingRows.svelte'
    import RisuNestPerformanceSettings from './RisuNestPerformanceSettings.svelte'
    import RisuNestStorageDashboard from './RisuNestStorageDashboard.svelte'
    import RisuNestBackupRestore from './RisuNestBackupRestore.svelte'
    import RisuNestIOSPlatform from './RisuNestIOSPlatform.svelte'
    import RisuNestAndroidPlatform from './RisuNestAndroidPlatform.svelte'
    import RisuNestLogViewer from './RisuNestLogViewer.svelte'
    import ServerSyncSettings from './ServerSyncSettings.svelte'
    import RisuNestUpdateSettings from './RisuNestUpdateSettings.svelte'

    const sections: { id: string; label: string }[] = [
        { id: 'risunest-perf', label: language.risuNest.perf.title },
        { id: 'risunest-streaming', label: language.risuNest.streaming.title },
        { id: 'risunest-inlay', label: language.risuNest.inlay.title },
        ...(isTauri ? [{ id: 'risunest-update', label: language.risuNest.update.title }] : []),
        ...(isTauri ? [{ id: 'risunest-server-sync', label: language.risuNest.serverSync.title }] : []),
        ...(isTauri ? [{ id: 'risunest-storage', label: language.risuNest.storage.title }] : []),
        { id: 'risunest-backup', label: language.risuNest.backup.title },
        ...(isTauriAndroid || isTauriIOS ? [{ id: 'risunest-platform', label: language.risuNest.platform.title }] : []),
        ...(isTauri ? [{ id: 'risunest-diag', label: language.risuNest.diag.title }] : []),
    ]

    function jumpTo(id: string): void {
        document.getElementById(id)?.scrollIntoView({ block: 'start', behavior: 'smooth' })
    }
</script>

<div class="@container w-full max-w-3xl">
    <h1 class="text-2xl font-bold">{language.risuNest.menuTitle}</h1>
    <nav class="-mx-4 mt-3 flex gap-1.5 overflow-x-auto px-4 pb-1 [scrollbar-width:none] @xl:mx-0 @xl:flex-wrap @xl:px-0" aria-label={language.risuNest.sectionNav}>
        {#each sections as section (section.id)}
            <button type="button" class="shrink-0 rounded-full border border-darkborderc px-2.5 py-0.5 text-xs text-textcolor2 transition-colors duration-200 hover:bg-selected hover:text-textcolor focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected" onclick={() => jumpTo(section.id)}>{section.label}</button>
        {/each}
    </nav>
    <RisuNestPerformanceSettings />
    <RisuNestSettingRows items={risuNestSettingsItems} />
    {#if isTauri}
        <RisuNestUpdateSettings />
    {/if}
    {#if isTauri}
        <section id="risunest-server-sync" class="scroll-mt-4"><ServerSyncSettings /></section>
    {/if}
    {#if isTauri}
        <RisuNestStorageDashboard />
    {/if}
    <RisuNestBackupRestore />
    {#if isTauriIOS}
        <RisuNestIOSPlatform />
    {/if}
    {#if isTauriAndroid}
        <RisuNestAndroidPlatform />
    {/if}
    {#if isTauri}
        <RisuNestLogViewer />
    {/if}
</div>
