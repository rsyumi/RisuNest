<script lang="ts">
    import { onMount } from 'svelte'
    import { invoke } from '@tauri-apps/api/core'
    import { language } from 'src/lang'
    import { alertConfirm } from 'src/ts/alert'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import Button from 'src/lib/UI/GUI/Button.svelte'

    interface Integration { available: boolean; registered: boolean; replacesExisting: boolean; token: string; error?: string }
    let integration = $state<Integration>()
    let busy = $state(false)
    let error = $state('')
    const text = $derived(language.risuNest.platform)
    const refresh = () => invoke<Integration>('appimage_integration_state')
    onMount(() => { void refresh().then(value => { integration = value; error = value.error ?? '' }).catch(() => {}) })
    async function integrate() {
        if (busy) return
        busy = true
        error = ''
        try {
            const current = await refresh()
            integration = current
            if (current.error) throw new Error(current.error)
            if (!current.available) return
            if (current.replacesExisting && !await alertConfirm(text.appImageReplace)) return
            await invoke('appimage_integration_register', { token: current.token, replaceExisting: current.replacesExisting })
            integration = await refresh()
        } catch (cause) { error = cause instanceof Error ? cause.message : String(cause) }
        finally { busy = false }
    }
</script>

{#if integration?.available}
    <SettingGroup title={text.title} id="risunest-appimage">
        <SettingRow label={text.appImageLinks} help={text.appImageLinksHelp}>
            <Button disabled={busy} onclick={() => void integrate()}>{text.appImageRegister}</Button>
        </SettingRow>
        {#if integration.registered}<p class="px-3 py-2 text-sm">{text.appImageRegistered}</p>{/if}
        {#if error}<p role="status" class="px-3 py-2 text-sm text-textcolor2">{error}</p>{/if}
    </SettingGroup>
{/if}
