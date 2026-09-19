<script lang="ts">
    import SettingButton from './SettingButton.svelte'

    let { perform, disabled = false }: { perform: () => Promise<void>; disabled?: boolean } = $props()
    let busy = $state(false)

    async function run() {
        busy = true
        try {
            await perform()
        } finally {
            busy = false
        }
    }
</script>

<SettingButton {busy} {disabled} onclick={run} name="save" title="Save synthetic settings">
    Save settings
</SettingButton>
