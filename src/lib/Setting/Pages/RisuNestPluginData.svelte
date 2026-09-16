<script lang="ts">
    import { language } from 'src/lang'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import PluginDataManager from '../RisuNest/PluginDataManager.svelte'

    const strings = language.risuNest.pluginData
</script>

<SettingGroup
    id="risunest-plugin-data"
    title={strings.title}
    description={strings.description}
    divide={false}
>
    <PluginDataManager />
</SettingGroup>
