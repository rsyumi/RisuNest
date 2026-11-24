<script lang="ts">
    import PresetListPopup from '../../src/lib/UI/PresetListPopup.svelte'

    const presets = Array.from({ length: 8 }, (_, index) => ({ name: `Synthetic preset ${index + 1}` }))
    let query = $state('')
</script>

<PresetListPopup
    title="Synthetic presets"
    close={() => {}}
    list={() => presets}
    {presets}
    filter={(preset) => preset.name.includes(query)}
    apply={async () => {}}
    listScroll
>
    {#snippet header()}
        <input type="search" aria-label="Search presets" bind:value={query} />
    {/snippet}
</PresetListPopup>
