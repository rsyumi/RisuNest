<script lang="ts">
    import { untrack } from 'svelte'
    import { language } from 'src/lang'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
    import { doingAlert } from 'src/ts/alert'
    import { fillTemplate, formatBytes } from 'src/ts/gui/nativeFileJobDialogModel'
    import { pluginInstallDialog, type ActivePluginInstall } from 'src/ts/importIntake/pluginInstall'

    let panel = $state<HTMLDivElement | undefined>()
    let installingRequest = $state.raw<ActivePluginInstall | null>(null)

    const active = $derived($pluginInstallDialog)
    const busy = $derived(active !== null && installingRequest === active)
    const copy = $derived(language.risuNest.importIntake)

    const rows = $derived.by(() => {
        if (!active) return []
        const { header, replaces, fileName, bytes } = active.prompt
        const list: { label: string; value: string }[] = []
        if (replaces) {
            if (replaces.version) list.push({ label: copy.pluginInstalledVersion, value: replaces.version })
            if (header.version) list.push({ label: copy.pluginNewVersion, value: header.version })
        } else if (header.version) {
            list.push({ label: copy.pluginVersion, value: header.version })
        }
        if (header.updateHost) list.push({ label: copy.pluginUpdateURL, value: header.updateHost })
        const size = bytes === undefined ? '' : formatBytes(bytes)
        list.push({ label: copy.detailFile, value: size ? `${fileName} · ${size}` : fileName })
        return list
    })

    $effect(() => {
        if (active && panel) untrack(() => panel?.focus())
    })

    function install(): void {
        const request = active
        if (!request) return
        installingRequest = request
        void request.install()
    }

    function dismissBackdrop(event: MouseEvent): void {
        if (event.target === event.currentTarget) active?.cancel()
    }

    // Captured so a sheet below that also closes on Escape stays open.
    function handleKeydown(event: KeyboardEvent): void {
        if (!active || event.key !== 'Escape' || doingAlert()) return
        event.preventDefault()
        event.stopPropagation()
        active.cancel()
    }
</script>

<svelte:window onkeydowncapture={handleKeydown} />

{#if active}
    {@const prompt = active.prompt}
    <div role="presentation" class="fixed inset-0 z-[1100] flex items-center justify-center bg-black/60 p-4" onclick={dismissBackdrop}>
        <div
            bind:this={panel}
            tabindex="-1"
            role="dialog"
            aria-modal="true"
            aria-labelledby="plugin-install-dialog-title"
            data-testid="plugin-install-dialog"
            class="flex max-h-[90dvh] w-full max-w-lg flex-col gap-4 overflow-y-auto rounded-lg border border-darkborderc bg-darkbg p-5 text-textcolor outline-hidden">
            <h2 id="plugin-install-dialog-title" class="text-lg font-bold">{prompt.replaces ? copy.pluginReplaceTitle : copy.pluginInstallTitle}</h2>
            <dl class="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 rounded-md border border-darkborderc bg-bgcolor px-3 py-2.5 text-sm">
                <dt class="text-subtext">{language.name}</dt>
                <dd class="min-w-0 break-words">
                    {prompt.header.displayName ?? prompt.header.name}
                    {#if prompt.header.displayName}
                        <span class="block font-mono text-xs text-subtext">{prompt.header.name}</span>
                    {/if}
                </dd>
                {#each rows as row (row.label)}
                    <dt class="text-subtext">{row.label}</dt>
                    <dd class="min-w-0 break-words tabular-nums">{row.value}</dd>
                {/each}
            </dl>
            {#if prompt.replaces}
                <p class="text-sm">{fillTemplate(copy.pluginReplaceData, prompt.replaces.displayName ?? prompt.replaces.name)}</p>
            {/if}
            <div class="rounded-md border border-danger-400/50 bg-danger-400/5 px-3 py-2 text-[13px] leading-normal">{`${copy.pluginTrustedSources} ${language.pluginWarn}`}</div>
            <footer class="flex flex-wrap items-center justify-end gap-2">
                <SettingButton variant="secondary" disabled={busy} onclick={() => active?.cancel()}>{language.cancel}</SettingButton>
                <SettingButton {busy} onclick={install}>{prompt.replaces ? copy.replace : copy.install}</SettingButton>
            </footer>
        </div>
    </div>
{/if}
