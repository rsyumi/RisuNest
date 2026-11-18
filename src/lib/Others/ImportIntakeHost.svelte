<script lang="ts">
    import { onMount } from 'svelte'
    import { installImportIntake } from 'src/ts/importIntake/intake'
    import ImportDropOverlay from './ImportDropOverlay.svelte'
    import ImportIntakeSheet from './ImportIntakeSheet.svelte'
    import PluginInstallDialog from './PluginInstallDialog.svelte'

    onMount(() => installImportIntake())
</script>

<ImportIntakeSheet />
<PluginInstallDialog />
<ImportDropOverlay />
