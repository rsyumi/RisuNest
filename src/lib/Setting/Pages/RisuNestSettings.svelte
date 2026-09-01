<script lang="ts">
    import SettingRenderer from '../SettingRenderer.svelte'
    import { risuNestSettingsItems } from 'src/ts/setting/risuNestSettingsData'
    import RisuNestPerformanceSettings from './RisuNestPerformanceSettings.svelte'
    import RisuNestBackupRestore from './RisuNestBackupRestore.svelte'
</script>

<RisuNestPerformanceSettings />
<SettingRenderer items={risuNestSettingsItems} />
<RisuNestBackupRestore />
