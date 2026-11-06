<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { language } from 'src/lang'
    import { alertMd } from 'src/ts/alertQueue'
    import { loadDiagnosticInfo, formatDiagnosticLog } from 'src/ts/diagnostics'
    import { consoleLog, type ConsoleLogEntry } from 'src/ts/consoleLog'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import {
        getNativeLogFilePath,
        getNativeLogTail,
        NATIVE_LOG_NEWEST_PAGE,
        NATIVE_LOG_RUN_START_TARGET,
        readNativeLogFilePage,
        setNativeLogFileEnabled,
        type NativeLogEntry,
        type NativeLogFileCursor,
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
    type LogKind = 'native' | 'console'
    let pending = $state<'view' | 'copy' | 'consoleView' | 'consoleCopy' | 'savedView' | 'savedCopy' | 'savedPage' | null>(null)
    let consoleRevision = 0
    let viewRequest = 0
    let copyRequest = 0
    let clipboardWriteQueue = Promise.resolve()

    /** The saved log page on screen, newest first; empty until the first page loads. */
    let savedOpen = $state(false)
    let savedEntries = $state.raw<NativeLogEntry[]>([])
    let savedCursor = $state<NativeLogFileCursor>(NATIVE_LOG_NEWEST_PAGE)
    let savedOlder = $state<NativeLogFileCursor | null>(null)
    /** Cursors of the newer pages, the nearest last. */
    let savedNewer = $state<NativeLogFileCursor[]>([])
    let savedError = $state('')
    let savedRequest = 0
    let savedList = $state<HTMLElement>()

    type SavedRow =
        | { kind: 'date', key: string, label: string }
        | { kind: 'run', key: string, entry: NativeLogEntry }
        | { kind: 'entry', key: string, entry: NativeLogEntry }

    const savedRows = $derived.by(() => {
        const rows: SavedRow[] = []
        let day = ''
        savedEntries.forEach((entry, index) => {
            if (entry.level) {
                const date = new Date(entry.tsMs)
                const entryDay = date.toDateString()
                if (entryDay !== day) {
                    day = entryDay
                    rows.push({ kind: 'date', key: `date-${index}`, label: date.toLocaleDateString(undefined, { year: 'numeric', month: 'long', day: 'numeric', weekday: 'short' }) })
                }
            }
            rows.push({ kind: entry.target === NATIVE_LOG_RUN_START_TARGET && entry.level ? 'run' : 'entry', key: `entry-${index}`, entry })
        })
        return rows
    })

    function formatEntry(entry: NativeLogEntry | ConsoleLogEntry) {
        return `[${new Date(entry.tsMs).toISOString()}] [${entry.level}] [${'source' in entry ? entry.source : entry.target}] ${entry.message}`
    }

    function formatLog(logEntries: (NativeLogEntry | ConsoleLogEntry)[]) {
        return logEntries
        .slice()
        .reverse()
        .sort((left, right) => right.tsMs - left.tsMs)
        .map(formatEntry)
        .join('\n')
    }

    /** Saved entries keep file order; a line the reader could not split is copied as it is. */
    function formatSavedLog(logEntries: NativeLogEntry[]) {
        return logEntries.map((entry) => entry.level ? formatEntry(entry) : entry.message).join('\n')
    }

    /** Local 24-hour time with milliseconds, the same in every locale. */
    function clockTime(tsMs: number, milliseconds = true) {
        const date = new Date(tsMs)
        const pad = (value: number) => String(value).padStart(2, '0')
        const time = `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
        return milliseconds ? `${time}.${String(date.getMilliseconds()).padStart(3, '0')}` : time
    }

    function levelTone(level: string) {
        if (level === 'error' || level === 'panic') return 'error'
        if (level === 'warn') return 'warn'
        return 'info'
    }

    /**
     * Log lines are shown through a markdown renderer, so they have to be fenced
     * to stay verbatim. The fence is a run of tildes that is always longer than
     * any tilde run inside the log itself, so log content can never close it.
     */
    function fenceLog(text: string) {
        let longestTildeRun = 0
        for (const match of text.matchAll(/~+/g)) longestTildeRun = Math.max(longestTildeRun, match[0].length)
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
        savedRequest++
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

    function readLog(kind: LogKind) {
        if (kind === 'console') return consoleLog.snapshot()
        return getNativeLogTail()
    }

    function emptyLog(kind: LogKind) {
        return kind === 'console' ? language.risuNest.diag.consoleEmpty : language.risuNest.diag.logEmpty
    }

    function clearConsole() {
        consoleLog.clear()
        consoleRevision++
        if (pending === 'consoleView' || pending === 'consoleCopy') pending = null
    }

    async function viewLog(kind: LogKind = 'native') {
        const request = ++viewRequest
        const revision = consoleRevision
        pending = kind === 'console' ? 'consoleView' : 'view'
        try {
            const [freshEntries, info] = await Promise.all([readLog(kind), loadDiagnosticInfo()])
            if (request !== viewRequest || (kind === 'console' && revision !== consoleRevision)) return
            if (kind === 'native') {
                entries = freshEntries as NativeLogEntry[]
                logLoaded = true
            }
            errorMessage = ''
            const text = formatDiagnosticLog(info, formatLog(freshEntries))
            alertMd(text ? fenceLog(text) : emptyLog(kind), true)
        } catch {
            if (request === viewRequest) errorMessage = language.risuNest.diag.actionFailed
        } finally {
            if (request === viewRequest) pending = null
        }
    }

    async function copyLog(kind: LogKind = 'native') {
        const request = ++copyRequest
        const revision = consoleRevision
        pending = kind === 'console' ? 'consoleCopy' : 'copy'
        try {
            const [freshEntries, info] = await Promise.all([readLog(kind), loadDiagnosticInfo()])
            if (request !== copyRequest || (kind === 'console' && revision !== consoleRevision)) return
            if (kind === 'native') {
                entries = freshEntries as NativeLogEntry[]
                logLoaded = true
            }
            errorMessage = ''
            const text = formatDiagnosticLog(info, formatLog(freshEntries)) || emptyLog(kind)
            // The button is free again once the log is read; a slow clipboard
            // write is serialized below, so a repeat request cannot reorder it.
            pending = null
            await writeClipboard(text, () => request === copyRequest && (kind !== 'console' || revision === consoleRevision))
        } catch {
            if (request === copyRequest) errorMessage = language.risuNest.diag.actionFailed
        } finally {
            if (request === copyRequest) pending = null
        }
    }

    async function writeClipboard(text: string, current: () => boolean) {
        const pendingWrite = clipboardWriteQueue.then(async () => {
            if (!current()) return
            try {
                await navigator.clipboard.writeText(text)
            } catch {
                if (!current()) return
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
    }

    /** Shows one page of the saved log; `newer` holds the cursors to page back up. */
    async function showSavedPage(cursor: NativeLogFileCursor, newer: NativeLogFileCursor[], action: 'savedView' | 'savedPage') {
        const request = ++savedRequest
        pending = action
        try {
            const page = await readNativeLogFilePage(cursor)
            if (request !== savedRequest) return
            savedEntries = page.entries.slice().reverse()
            savedCursor = cursor
            savedOlder = page.older
            savedNewer = newer
            savedError = ''
            savedOpen = true
            savedList?.scrollTo?.({ top: 0 })
        } catch {
            if (request === savedRequest) savedError = language.risuNest.diag.actionFailed
        } finally {
            if (request === savedRequest && pending === action) pending = null
        }
    }

    function toggleSavedLog() {
        if (savedOpen) {
            savedRequest++
            savedOpen = false
            savedEntries = []
            if (pending === 'savedPage' || pending === 'savedView') pending = null
            return
        }
        void showSavedPage(NATIVE_LOG_NEWEST_PAGE, [], 'savedView')
    }

    function showOlderSavedPage() {
        if (savedOlder) void showSavedPage(savedOlder, [...savedNewer, savedCursor], 'savedPage')
    }

    function showNewerSavedPage() {
        const cursor = savedNewer.at(-1)
        if (cursor) void showSavedPage(cursor, savedNewer.slice(0, -1), 'savedPage')
    }

    /** Copies the page on screen, or the newest page while the viewer is closed. */
    async function copySavedLog() {
        const request = ++copyRequest
        pending = 'savedCopy'
        try {
            const shown = savedOpen ? savedEntries : null
            const [logEntries, info] = await Promise.all([
                shown ?? readNativeLogFilePage().then((page) => page.entries.slice().reverse()),
                loadDiagnosticInfo(),
            ])
            if (request !== copyRequest) return
            savedError = ''
            const text = formatDiagnosticLog(info, formatSavedLog(logEntries)) || language.risuNest.diag.savedLogEmpty
            pending = null
            await writeClipboard(text, () => request === copyRequest)
        } catch {
            if (request === copyRequest) savedError = language.risuNest.diag.actionFailed
        } finally {
            if (request === copyRequest) pending = null
        }
    }

    function savedRange() {
        const times = savedEntries.filter((entry) => entry.level).map((entry) => entry.tsMs)
        if (times.length === 0) return ''
        const day = (tsMs: number) => new Date(tsMs).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
        const oldest = Math.min(...times)
        const newest = Math.max(...times)
        const start = `${day(oldest)} ${clockTime(oldest, false)}`
        if (oldest === newest) return start
        return `${start} – ${day(newest) === day(oldest) ? '' : `${day(newest)} `}${clockTime(newest, false)}`
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
                <p class="mt-1 text-sm text-danger-400" role="alert" aria-live="assertive">{errorMessage}</p>
            {:else if logLoaded && entries.length === 0}
                <p class="mt-1 text-sm text-textcolor2" role="status" aria-live="polite">{language.risuNest.diag.logEmpty}</p>
            {/if}
        {/snippet}
        <SettingButton data-view-log busy={pending === 'view'} onclick={() => void viewLog()}>
            {language.risuNest.diag.viewLog}
        </SettingButton>
        <SettingButton data-copy-log variant="secondary" busy={pending === 'copy'} onclick={() => void copyLog()}>
            {language.risuNest.diag.copyLog}
        </SettingButton>
    </SettingRow>
    <div>
        <SettingRow label={language.risuNest.diag.savedLogTitle} help={language.risuNest.diag.savedLogHelp}>
            {#snippet below()}
                {#if savedError}
                    <p class="mt-1 text-sm text-danger-400" role="alert" aria-live="assertive">{savedError}</p>
                {/if}
            {/snippet}
            <SettingButton
                data-view-saved-log
                aria-expanded={savedOpen}
                aria-controls="risunest-saved-log"
                busy={pending === 'savedView'}
                onclick={toggleSavedLog}
            >
                {savedOpen ? language.close : language.risuNest.diag.viewLog}
            </SettingButton>
            <SettingButton data-copy-saved-log variant="secondary" busy={pending === 'savedCopy'} onclick={() => void copySavedLog()}>
                {language.risuNest.diag.copyLog}
            </SettingButton>
        </SettingRow>
        {#if savedOpen}
            <div id="risunest-saved-log" class="saved-log mx-4 mb-4 overflow-hidden rounded-md border border-darkborderc bg-bgcolor" data-saved-log>
                {#if savedEntries.length === 0}
                    <p class="px-3 py-6 text-center text-sm text-textcolor2" role="status">{language.risuNest.diag.savedLogEmpty}</p>
                {:else}
                    <div class="flex flex-wrap items-center justify-between gap-x-3 gap-y-1.5 border-b border-darkborderc/55 px-3 py-2">
                        <span class="min-w-0 mono text-xs text-textcolor2" data-saved-log-range>{savedRange()}</span>
                        {#if savedOlder || savedNewer.length > 0}
                            <div class="ml-auto flex gap-1.5">
                                <SettingButton data-saved-log-newer size="sm" variant="secondary" disabled={savedNewer.length === 0 || pending === 'savedPage'} onclick={showNewerSavedPage}>
                                    {language.risuNest.diag.newerPage}
                                </SettingButton>
                                <SettingButton data-saved-log-older size="sm" variant="secondary" disabled={!savedOlder || pending === 'savedPage'} onclick={showOlderSavedPage}>
                                    {language.risuNest.diag.olderPage}
                                </SettingButton>
                            </div>
                        {/if}
                    </div>
                    <ol
                        bind:this={savedList}
                        class="max-h-[min(28rem,60vh)] overflow-y-auto"
                        aria-label={language.risuNest.diag.savedLogList}
                        aria-busy={pending === 'savedPage' ? 'true' : undefined}
                    >
                        {#each savedRows as row (row.key)}
                            {#if row.kind === 'date'}
                                <li class="date sticky top-0 z-10 border-b border-darkborderc/40 px-3 py-1 text-[11px] font-medium text-textcolor2" data-saved-log-date>{row.label}</li>
                            {:else if row.kind === 'run'}
                                <li class="run flex items-center gap-2.5 px-3 py-2 text-[11px] text-textcolor2" data-saved-log-run>
                                    <span class="rule h-px min-w-3 flex-1" aria-hidden="true"></span>
                                    <span class="shrink-0 font-semibold text-textcolor">{language.risuNest.diag.appStarted}</span>
                                    <span class="min-w-0 truncate mono">{row.entry.message}</span>
                                    <time class="shrink-0 mono" datetime={new Date(row.entry.tsMs).toISOString()}>{clockTime(row.entry.tsMs)}</time>
                                    <span class="rule h-px min-w-3 flex-1" aria-hidden="true"></span>
                                </li>
                            {:else if row.entry.level}
                                <li class="entry" data-tone={levelTone(row.entry.level)} data-saved-log-entry>
                                    <time class="time mono text-[11px] text-textcolor2" datetime={new Date(row.entry.tsMs).toISOString()}>{clockTime(row.entry.tsMs)}</time>
                                    <span class="level mono text-[11px]">{row.entry.level}</span>
                                    <span class="target min-w-0 mono text-[11px] break-all text-textcolor2">{row.entry.target}</span>
                                    <p class="message min-w-0 mono text-xs leading-relaxed whitespace-pre-wrap text-textcolor">{row.entry.message}</p>
                                </li>
                            {:else}
                                <li class="entry raw" data-saved-log-entry>
                                    <p class="message min-w-0 mono text-xs leading-relaxed whitespace-pre-wrap text-textcolor2">{row.entry.message}</p>
                                </li>
                            {/if}
                        {/each}
                    </ol>
                {/if}
            </div>
        {/if}
    </div>
    <SettingRow label={language.risuNest.diag.consoleTitle} help={language.risuNest.diag.consoleHelp}>
        <SettingButton data-view-console-log busy={pending === 'consoleView'} onclick={() => void viewLog('console')}>
            {language.risuNest.diag.viewLog}
        </SettingButton>
        <SettingButton data-copy-console-log variant="secondary" busy={pending === 'consoleCopy'} onclick={() => void copyLog('console')}>
            {language.risuNest.diag.copyLog}
        </SettingButton>
        <SettingButton data-clear-console-log variant="secondary" onclick={clearConsole}>
            {language.risuNest.diag.clearLog}
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

<style>
    /* The app font is set on every element, so the log text names its own. */
    .mono {
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    }
    .saved-log .date {
        background: var(--risu-theme-bgcolor);
    }
    .saved-log .run {
        border-top: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 55%, transparent);
        background: var(--risu-theme-darkbg);
    }
    .saved-log .rule {
        background: var(--risu-theme-darkborderc);
    }
    /* Narrow panels put time, level and target on one line and the message under them. */
    .entry {
        display: grid;
        grid-template-columns: auto auto minmax(0, 1fr);
        grid-template-areas:
            'time level target'
            'message message message';
        align-items: baseline;
        column-gap: 0.5rem;
        row-gap: 0.125rem;
        padding: 0.5rem 0.75rem 0.5rem 0.625rem;
        border-top: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 40%, transparent);
        border-left: 2px solid transparent;
    }
    .entry.raw {
        grid-template-areas: 'message message message';
    }
    .time { grid-area: time; }
    .level { grid-area: level; }
    .target { grid-area: target; }
    .message {
        grid-area: message;
        overflow-wrap: anywhere;
    }
    /* Wide panels keep time and level in their own columns so a page reads down the clock. */
    @container (min-width: 36rem) {
        .entry {
            grid-template-columns: 6.25rem 2.75rem minmax(0, 1fr);
            grid-template-areas:
                'time level target'
                'time level message';
            column-gap: 0.75rem;
        }
    }
    .entry[data-tone='error'] {
        border-left-color: var(--risu-theme-draculared);
    }
    .entry[data-tone='error'] .level {
        color: var(--risu-theme-draculared);
        font-weight: 600;
    }
    .entry[data-tone='warn'] {
        border-left-color: color-mix(in srgb, var(--risu-theme-textcolor2) 70%, transparent);
    }
    .entry[data-tone='warn'] .level {
        color: var(--risu-theme-textcolor);
        font-weight: 600;
    }
    .entry[data-tone='info'] .level {
        color: var(--risu-theme-textcolor2);
    }
</style>
