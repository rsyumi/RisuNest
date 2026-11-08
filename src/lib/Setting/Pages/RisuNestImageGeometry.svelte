<script lang="ts">
    import { language } from 'src/lang'
    import { imageGeometryController as job } from 'src/ts/process/files/imageGeometryController.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingProgress from '../RisuNest/SettingProgress.svelte'

    const strings = $derived(language.risuNest.imageGeometry)
    const counts = $derived(strings.counts.replace('{saved}', String(job.progress.saved))
        .replace('{skipped}', String(job.progress.skipped)).replace('{failed}', String(job.progress.failed)))
    const status = $derived(job.failed ? strings.failed : job.result?.cancelled ? strings.cancelled
        : job.result?.catalogChanged ? strings.catalogChanged : strings.complete)
</script>

<SettingGroup title={strings.title}>
    <SettingRow label={strings.calculate} help={strings.help}>
        {#snippet below()}
            {#if job.running}
                <div class="mt-2">
                    <SettingProgress label={strings.calculate} detail={counts} fraction={null} />
                </div>
            {:else if job.result || job.failed}
                <p class="mt-1 text-sm text-subtext" role="status" aria-live="polite">{status} {counts}</p>
            {/if}
        {/snippet}
        {#if job.running}
            <SettingButton variant="secondary" disabled={job.cancelRequested} onclick={() => job.cancel()}>{language.cancel}</SettingButton>
        {:else}
            <SettingButton onclick={() => job.start()}>{strings.calculate}</SettingButton>
        {/if}
    </SettingRow>
</SettingGroup>
