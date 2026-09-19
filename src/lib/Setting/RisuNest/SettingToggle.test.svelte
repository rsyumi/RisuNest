<script lang="ts">
    import SettingToggle from './SettingToggle.svelte'

    let { onchange, disabled = false }: { onchange: (checked: boolean) => void; disabled?: boolean } = $props()
    let checked = $state(false)
</script>

<SettingToggle bind:checked {onchange} {disabled} label="Keep synthetic history" />
<output aria-label="Bound setting">{String(checked)}</output>
