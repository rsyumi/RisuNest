<script lang="ts">
    import { language } from 'src/lang'
    import { diagnosticInfo, formatErrorDetails } from 'src/ts/diagnostics'
    import { alertToast } from 'src/ts/alert'
    import { isTauri, isTauriAndroid, isTauriIOS } from 'src/ts/platform'
    import { nativeRoots } from 'src/ts/storage/nativePaths'
    import { exportOriginalData } from 'src/ts/storage/rawRecoveryExport'
    import type { BootFailure } from 'src/ts/stores.svelte'

    let { failure }: { failure: BootFailure } = $props()

    const copy = language.risuNest.boot
    let dataFolder = $state('')
    let exportMessage = $state('')

    const explanation = (value: BootFailure) => {
        switch (value.kind) {
            case 'schema-unsupported': return copy.schemaUnsupported
            case 'store-open': return copy.storeOpen
            default: return copy.unknown
        }
    }

    $effect(() => {
        if (failure.kind !== 'schema-unsupported' || !isTauri || isTauriAndroid || isTauriIOS) return
        let current = true
        void nativeRoots().then((roots) => { if (current) dataFolder = roots.data }, () => {})
        return () => { current = false }
    })

    const exportData = async () => {
        exportMessage = ''
        try {
            const result = await exportOriginalData()
            if (!result) return
            exportMessage = result.warningCodes.includes('source-problems')
                ? language.risuNest.recovery.exportPartial
                : language.risuNest.recovery.exportComplete
        } catch (error) {
            exportMessage = error instanceof DOMException && error.name === 'AbortError'
                ? language.risuNest.recovery.exportCancelled
                : language.risuNest.recovery.exportFailed
        }
    }

    const details = (value: BootFailure) => formatErrorDetails($diagnosticInfo, [
        copy.title,
        value.message,
        value.stage ? `${copy.stage}: ${value.stage}` : '',
    ].filter((line) => line !== '').join('\n'))

    const copyDetails = async (value: BootFailure) => {
        const text = details(value)
        try {
            await navigator.clipboard.writeText(text)
        } catch {
            const textarea = document.createElement('textarea')
            textarea.value = text
            document.body.appendChild(textarea)
            textarea.select()
            try {
                document.execCommand('copy')
            } finally {
                document.body.removeChild(textarea)
            }
        }
        alertToast(copy.copied)
    }
</script>

<div class="w-full h-full overflow-y-auto bg-darkbg text-textcolor flex justify-center items-start">
    <div class="w-full max-w-xl flex flex-col p-4 sm:p-6 gap-3">
        <h1 class="text-xl font-bold">{copy.title}</h1>
        <p class="text-sm text-subtext">{explanation(failure)}</p>
        {#if failure.kind === 'schema-unsupported' && (isTauriAndroid || isTauriIOS || dataFolder)}
            <div class="flex flex-col gap-1 text-xs text-subtext border border-darkborderc rounded-md p-3">
                {#if isTauriAndroid}
                    <span>{copy.dataPathAndroid}</span>
                {:else if isTauriIOS}
                    <span>{copy.dataPathIos}</span>
                {:else}
                    <span>{copy.dataFolder}</span>
                    <span class="select-text break-all font-mono">{dataFolder}</span>
                {/if}
            </div>
        {/if}
        <code class="text-xs font-mono select-text break-all whitespace-pre-wrap border border-darkborderc rounded-md p-3 text-subtext">{details(failure)}</code>
        {#if isTauri && failure.stage !== 'native-setup'}
            <div class="rounded-md border border-darkborderc p-3">
                <p class="text-sm font-bold">{language.risuNest.recovery.exportTitle}</p>
                <p class="mt-1 text-xs text-subtext">{language.risuNest.recovery.exportHelp}</p>
                <button
                    class="mt-2 bg-darkbutton border border-darkborderc rounded-md px-4 py-2 text-sm hover:bg-selected"
                    onclick={exportData}
                >{language.risuNest.recovery.exportAction}</button>
                {#if exportMessage}
                    <p class="mt-2 text-xs text-subtext" role="status">{exportMessage}</p>
                {/if}
            </div>
        {/if}
        <div class="flex flex-wrap gap-2 mt-1">
            <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 text-sm hover:bg-selected" onclick={() => location.reload()}>
                {copy.restart}
            </button>
            <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 text-sm hover:bg-selected" onclick={() => copyDetails(failure)}>
                {copy.copyDetails}
            </button>
        </div>
    </div>
</div>
