<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { language } from 'src/lang'
    import { alertMd } from 'src/ts/alert'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
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
    let logLoaded = $state(false)
    let errorMessage = $state('')
    let fileLogEnabled = $state(getDeviceSettings().nativeFileLogEnabled)
    let fileLogPath = $state('')
    let fileLogUpdatePending = $state(false)
    let pending = $state<'view' | 'copy' | null>(null)
    let viewRequest = 0
    let copyRequest = 0
    let clipboardWriteQueue = Promise.resolve()

    function formatLog(logEntries: NativeLogEntry[]) {
        return logEntries
        .slice()
        .reverse()
        .map((entry) => `[${new Date(entry.tsMs).toISOString()}] [${entry.level}] ${entry.message}`)
        .join('\n')
    }

    /**
     * Log lines are shown through a markdown renderer, so they have to be fenced
     * to stay verbatim. The fence is a run of tildes that is always longer than
     * any tilde run inside the log itself, so log content can never close it.
     */
    function fenceLog(text: string) {
        const tildeRuns: string[] = text.match(/~+/g) ?? []
        const longestTildeRun = tildeRuns
            .reduce((longest, run) => Math.max(longest, run.length), 0)
        const fence = '~'.repeat(Math.max(4, longestTildeRun + 1))
        return `${fence}\n${text}\n${fence}`
    }

    const unsubscribe = subscribeDeviceSettings((settings) => {
        fileLogEnabled = settings.nativeFileLogEnabled
        if (fileLogEnabled && !fileLogUpdatePending) void loadFilePath()
    })

    onDestroy(() => {
        unsubscribe()
        viewRequest++
        copyRequest++
    })

    onMount(() => {
        if (fileLogEnabled) void loadFilePath()
    })

    async function loadFilePath() {
        try {
            fileLogPath = await getNativeLogFilePath()
        } catch {
            errorMessage = language.risuNest.diag.actionFailed
        }
    }

    async function viewLog() {
        const request = ++viewRequest
        pending = 'view'
        try {
            const freshEntries = await getNativeLogTail()
            if (request !== viewRequest) return
            entries = freshEntries
            logLoaded = true
            errorMessage = ''
            const text = formatLog(freshEntries)
            alertMd(text ? fenceLog(text) : language.risuNest.diag.logEmpty)
        } catch {
            if (request === viewRequest) errorMessage = language.risuNest.diag.actionFailed
        } finally {
            if (request === viewRequest) pending = null
        }
    }

    async function copyLog() {
        const request = ++copyRequest
        pending = 'copy'
        try {
            const freshEntries = await getNativeLogTail()
            if (request !== copyRequest) return
            entries = freshEntries
            logLoaded = true
            errorMessage = ''
            const text = formatLog(freshEntries) || language.risuNest.diag.logEmpty
            // The button is free again once the log is read; a slow clipboard
            // write is serialized below, so a repeat request cannot reorder it.
            pending = null
            const pendingWrite = clipboardWriteQueue.then(async () => {
                if (request !== copyRequest) return
                try {
                    await navigator.clipboard.writeText(text)
                } catch {
                    if (request !== copyRequest) return
                    const textarea = document.createElement('textarea')
                    textarea.value = text
                    document.body.appendChild(textarea)
                    textarea.select()
                    try {
                        if (!document.execCommand('copy')) throw new Error('copy failed')
                    } finally {
                        document.body.removeChild(textarea)
                    }
                }
            })
            clipboardWriteQueue = pendingWrite.catch(() => undefined)
            await pendingWrite
        } catch {
            if (request === copyRequest) errorMessage = language.risuNest.diag.actionFailed
        } finally {
            if (request === copyRequest) pending = null
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
            errorMessage = language.risuNest.diag.actionFailed
        } finally {
            fileLogUpdatePending = false
        }
    }
</script>

<SettingGroup id="risunest-diag" title={language.risuNest.diag.title}>
    <SettingRow label={language.risuNest.diag.logTitle} help={language.risuNest.diag.logHelp}>
        {#snippet below()}
            {#if errorMessage}
                <p class="mt-1 text-sm text-draculared" role="alert" aria-live="assertive">{errorMessage}</p>
            {:else if logLoaded && entries.length === 0}
                <p class="mt-1 text-sm text-textcolor2" role="status" aria-live="polite">{language.risuNest.diag.logEmpty}</p>
            {/if}
        {/snippet}
        <SettingButton data-view-log busy={pending === 'view'} onclick={viewLog}>
            {language.risuNest.diag.viewLog}
        </SettingButton>
        <SettingButton data-copy-log busy={pending === 'copy'} onclick={() => void copyLog()}>
            {language.risuNest.diag.copyLog}
        </SettingButton>
    </SettingRow>
    <SettingRow inline label={language.risuNest.diag.fileLog} help={language.risuNest.diag.fileLogHelp}>
        {#snippet below()}
            {#if fileLogEnabled && fileLogPath}
                <code class="mt-1 block text-xs break-all text-textcolor2" role="status" aria-live="polite">{fileLogPath}</code>
            {/if}
        {/snippet}
        <SettingToggle
            checked={fileLogEnabled}
            disabled={fileLogUpdatePending}
            label={language.risuNest.diag.fileLog}
            onchange={(enabled) => void changeFileLogging(enabled)}
        />
    </SettingRow>
</SettingGroup>
