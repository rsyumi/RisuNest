<script lang="ts">
    import SettingRenderer from '../SettingRenderer.svelte'
    import { risuNestSettingsItems } from 'src/ts/setting/risuNestSettingsData'
    import { isTauri, isTauriAndroid } from 'src/ts/platform'
    import RisuNestPerformanceSettings from './RisuNestPerformanceSettings.svelte'
    import RisuNestStorageDashboard from './RisuNestStorageDashboard.svelte'
    import RisuNestBackupRestore from './RisuNestBackupRestore.svelte'
    import RisuNestAndroidPlatform from './RisuNestAndroidPlatform.svelte'
    import RisuNestLogViewer from './RisuNestLogViewer.svelte'
</script>

<RisuNestPerformanceSettings />
<SettingRenderer items={risuNestSettingsItems} />
{#if isTauri}
    <RisuNestStorageDashboard />
{/if}
<RisuNestBackupRestore />
{#if isTauriAndroid}
    <RisuNestAndroidPlatform />
{/if}
{#if isTauri}
    <RisuNestLogViewer />
{/if}
