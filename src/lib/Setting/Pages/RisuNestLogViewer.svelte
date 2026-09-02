<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { language } from 'src/lang'
    import { alertMd } from 'src/ts/alert'
    import {
        getNativeLogFilePath,
        getNativeLogTail,
        setNativeLogFileEnabled,
        type NativeLogEntry,
    } from 'src/ts/nativeLog'
    import {
        getDeviceSettings,
        subscribeDeviceSettings,
        updateDeviceSettings,
    } from 'src/ts/storage/deviceSettings'

    let entries = $state<NativeLogEntry[]>([])
    let errorMessage = $state('')
    let fileLogEnabled = $state(getDeviceSettings().nativeFileLogEnabled)
    let fileLogPath = $state('')
    let fileLogUpdatePending = $state(false)
    let formattedLog = $derived(entries
        .slice()
        .reverse()
        .map((entry) => `[${new Date(entry.tsMs).toISOString()}] [${entry.level}] ${entry.message}`)
        .join('\n'))

    const unsubscribe = subscribeDeviceSettings((settings) => {
        fileLogEnabled = settings.nativeFileLogEnabled
        if (fileLogEnabled && !fileLogUpdatePending) void loadFilePath()
    })

    onDestroy(unsubscribe)

    onMount(() => {
        void refresh()
    })

    async function refresh() {
        try {
            entries = await getNativeLogTail()
            errorMessage = ''
        } catch {
            errorMessage = language.error
        }
        if (fileLogEnabled) await loadFilePath()
    }

    async function loadFilePath() {
        try {
            fileLogPath = await getNativeLogFilePath()
        } catch {
            errorMessage = language.error
        }
    }

    function viewLog() {
        alertMd(formattedLog || language.risuNest.diag.logEmpty)
    }

    async function copyLog() {
        const text = formattedLog || language.risuNest.diag.logEmpty
        try {
            await navigator.clipboard.writeText(text)
        } catch {
            const textarea = document.createElement('textarea')
            textarea.value = text
            document.body.appendChild(textarea)
            textarea.select()
            document.execCommand('copy')
            document.body.removeChild(textarea)
        }
    }

    async function changeFileLogging(enabled: boolean) {
        if (fileLogUpdatePending) return
        fileLogUpdatePending = true
        fileLogEnabled = enabled
        try {
            await setNativeLogFileEnabled(enabled)
            updateDeviceSettings({ nativeFileLogEnabled: enabled })
            fileLogEnabled = enabled
            if (enabled) await loadFilePath()
        } catch {
            fileLogEnabled = getDeviceSettings().nativeFileLogEnabled
            errorMessage = language.error
        } finally {
            fileLogUpdatePending = false
        }
    }
</script>

<section class="flex flex-col gap-2 text-textcolor">
    <h2 class="mb-2 text-2xl font-bold mt-2">{language.risuNest.diag.title}</h2>
    <div class="flex gap-2 flex-wrap">
        <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 focus-visible:outline focus-visible:outline-2 focus-visible:outline-darkborderc focus-visible:outline-offset-2" data-view-log onclick={viewLog}>
            {language.risuNest.diag.viewLog}
        </button>
        <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 focus-visible:outline focus-visible:outline-2 focus-visible:outline-darkborderc focus-visible:outline-offset-2" data-copy-log onclick={() => void copyLog()}>
            {language.risuNest.diag.copyLog}
        </button>
    </div>

    <label class="flex items-center gap-2 cursor-pointer rounded-md focus-within:outline focus-within:outline-2 focus-within:outline-darkborderc focus-within:outline-offset-2">
        <input
            class="sr-only"
            type="checkbox"
            checked={fileLogEnabled}
            disabled={fileLogUpdatePending}
            onchange={(event) => void changeFileLogging(event.currentTarget.checked)}
        />
        <span class="w-5 h-5 rounded-md border-2 border-darkborderc flex justify-center items-center" class:bg-darkborderc={fileLogEnabled}>
            {#if fileLogEnabled}✓{/if}
        </span>
        <span>{language.risuNest.diag.fileLog}</span>
    </label>
    <span class="text-textcolor2 text-sm">{language.risuNest.diag.fileLogHelp}</span>
    {#if fileLogEnabled && fileLogPath}
        <code class="text-textcolor2 text-sm break-all" role="status" aria-live="polite">{fileLogPath}</code>
    {/if}

    {#if errorMessage}
        <span class="text-draculared" role="alert" aria-live="assertive">{errorMessage}</span>
    {:else if entries.length === 0}
        <span class="text-textcolor2" role="status" aria-live="polite">{language.risuNest.diag.logEmpty}</span>
    {/if}
</section>
