<script lang="ts">
    import { onMount } from 'svelte'
    import { language } from 'src/lang'
    import { Copy, Download, Bookmark, Trash2, Search, RefreshCw, ChevronDown } from '@lucide/svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import {
        networkLog, downloadNetworkLog, logSize, parseSsePreview, prettyLogJson, groupNetworkLogItems,
        type NetworkLogSettings, type NetworkLogListing, type NetworkLogRecord,
        type LogBodyPage, type LogSide, type LogObservation, type LogArrivals,
    } from 'src/ts/network/networkLog'
    import { exportMayLeavePartialFile } from 'src/ts/storage/nativeExportFile'

    const t = language.risuNest.networkLog
    const budgets = [128, 256, 512, 1024, 2048, 4096, 8192]
    let settings = $state<NetworkLogSettings | null>(null)
    let listing = $state<NetworkLogListing | null>(null)
    let retained = $state(false)
    let search = $state('')
    let status = $state('all')
    let offset = $state(0)
    let selected = $state('')
    let detail = $state<NetworkLogRecord | null>(null)
    let tab = $state<'headers' | 'request' | 'response' | 'events'>('headers')
    let body = $state<LogBodyPage | null>(null)
    let bodyOffset = $state(0)
    let pageHistory = $state<{ offset: number; sequence: number; boundary: boolean; arrivalsFrom: number }[]>([])
    let sequence = $state(1)
    let boundary = $state(true)
    let arrivals = $state<(number | null)[]>([])
    let arrivalsFrom = 0
    let arrivalsNext = 0
    let observations = $state<LogObservation[]>([])
    let message = $state<LogObservation | null>(null)
    let messageBody = $state<LogBodyPage | null>(null)
    let pretty = $state(false)
    let busy = $state(false)
    let error = $state('')
    let notice = $state('')
    let mounted = false
    let listVersion = 0
    let detailVersion = 0
    let messageVersion = 0
    let searchTimer: ReturnType<typeof setTimeout> | undefined
    let noticeTimer: ReturnType<typeof setTimeout> | undefined
    let refreshing = false
    const selectedSide = $derived(tab === 'request' ? 'request' : 'response')
    const sseEvents = $derived(detail?.streamKind === 'sse' && body ? parseSsePreview(body.text, body.offset, !boundary) : [])
    const prettyText = $derived.by(() => {
        if (!body || body.binary || body.offset !== 0 || body.nextOffset < body.total) return null
        return prettyLogJson(body.text)
    })
    function reason(value: unknown): string {
        const code = value instanceof Error ? value.message : String(value)
        const reasons: Record<string, string> = {
            'network-log-clipboard-too-large': t.clipboardLarge, 'network-log-binary-body': t.clipboardBinary,
            'network-log-expired': t.expired, 'network-log-request-active': t.activeDelete,
            'network-log-invalid-destination': t.destinationInvalid, 'network-log-export-failed': t.exportFailed,
            'quota': t.quota, 'queue-full': t.queueFull, 'concurrent-limit': t.concurrentLimit,
            'write-failed': t.writeFailed, 'network-log-storage-failed': t.writeFailed,
            'binary-skipped': t.binarySkipped, 'network-log-readers-busy': t.readersBusy,
            'body-limit': t.bodyLimited, 'metadata-limit': t.partial, 'stopped': t.stopped,
            'started-late': t.startedLate, 'interrupted': t.interrupted,
        }
        return reasons[code] ?? t.error
    }
    function announce(value: string) {
        notice = value
        clearTimeout(noticeTimer)
        noticeTimer = setTimeout(() => { notice = '' }, 3000)
    }
    async function action(work: () => Promise<unknown>) {
        if (busy) return
        busy = true; error = ''; notice = ''
        try { await work() } catch (value) {
            error = exportMayLeavePartialFile(value) ? `${reason(value)} ${language.screenshotPartialDestinationMayRemain}` : reason(value)
        } finally { busy = false }
    }
    async function configure(patch: Partial<NetworkLogSettings>) {
        if (!settings) return
        await action(async () => {
            settings = await networkLog<NetworkLogSettings>({ op: 'configure', settings: { ...settings, ...patch } })
            await refresh(true)
        })
    }
    async function loadList() {
        const version = ++listVersion
        const result = await networkLog<NetworkLogListing>({ op: 'list', query: { retained, search, status, offset } })
        if (mounted && version === listVersion) listing = result
    }
    async function loadDetail() {
        const id = selected, currentTab = tab, currentOffset = bodyOffset, currentBoundary = boundary, currentFrom = arrivalsFrom, version = ++detailVersion
        if (!id) return
        const result = await networkLog<NetworkLogRecord>({ op: 'detail', id })
        let page: LogBodyPage | null = null
        let events: LogObservation[] = []
        let times: (number | null)[] = []
        let next = currentFrom
        if (currentTab !== 'headers') {
            page = currentTab === 'events' && result.streamKind === 'websocket'
                ? await networkLog<LogBodyPage>({ op: 'events', id, offset: currentOffset })
                : await networkLog<LogBodyPage>({ op: 'body', id, side: currentTab === 'request' ? 'request' : 'response', offset: currentOffset })
            if (currentTab === 'events' && result.streamKind === 'sse') {
                const parsed = parseSsePreview(page.text, page.offset, !currentBoundary)
                const found = await networkLog<LogArrivals>({ op: 'arrivalTimes', id, ends: parsed.map(event => event.endOffset), from: currentFrom })
                times = found.times; next = found.next
                const end = parsed.at(-1)?.endOffset
                if (end !== undefined && end < page.nextOffset) {
                    page = { ...page, text: new TextDecoder().decode(new TextEncoder().encode(page.text).subarray(0, end - page.offset)), nextOffset: end }
                }
            }
            if (currentTab === 'events' && result.streamKind === 'websocket') {
                events = page.text.trim().split('\n').filter(Boolean).map(line => JSON.parse(line) as LogObservation)
            }
        }
        if (mounted && selected === id && version === detailVersion) { detail = result; body = page; observations = events; arrivals = times; arrivalsNext = next }
    }
    // Polling rereads only records that can still change; actions pass `force` after changing one.
    async function refresh(force = false) {
        if (refreshing) return
        refreshing = true
        try { await loadList(); if (selected && (force || !detail || detail.state === 'active')) await loadDetail() }
        catch (value) { if (mounted) error = reason(value) }
        finally { refreshing = false }
    }
    function filter() {
        offset = 0; selected = ''; detail = null; body = null; detailVersion++
        void loadList().catch(value => { error = reason(value) })
    }
    function searchChanged() { clearTimeout(searchTimer); searchTimer = setTimeout(filter, 200) }
    function select(id: string) {
        selected = id; detail = null; body = null; tab = 'headers'; bodyOffset = 0; pageHistory = []; sequence = 1; boundary = true; arrivals = []; arrivalsFrom = 0; message = null; messageBody = null; error = ''
        void loadDetail().catch(value => { error = reason(value) })
    }
    function selectTab(value: typeof tab) {
        tab = value; body = null; bodyOffset = 0; pageHistory = []; sequence = 1; boundary = true; arrivals = []; arrivalsFrom = 0; pretty = false; message = null; messageBody = null
        void loadDetail().catch(value => { error = reason(value) })
    }
    function bodyPage(forward: boolean) {
        if (!body) return
        if (forward) {
            pageHistory = [...pageHistory, { offset: bodyOffset, sequence, boundary, arrivalsFrom }]
            if (tab === 'events' && detail?.streamKind === 'sse') {
                sequence += sseEvents.filter(event => event.complete).length
                boundary = sseEvents.at(-1)?.complete ?? boundary
                arrivalsFrom = arrivalsNext
            }
            bodyOffset = body.nextOffset
        } else {
            const previous = pageHistory.at(-1)
            bodyOffset = previous?.offset ?? 0; sequence = previous?.sequence ?? 1; boundary = previous?.boundary ?? true; arrivalsFrom = previous?.arrivalsFrom ?? 0
            pageHistory = pageHistory.slice(0, -1)
        }
        body = null; arrivals = []; message = null; messageBody = null
        void loadDetail().catch(value => { error = reason(value) })
    }
    async function copy(side: LogSide) {
        await action(async () => {
            const text = await networkLog<string>({ op: 'copy', id: selected, side })
            try { await navigator.clipboard.writeText(text) } catch { error = t.clipboardDenied; return }
            announce(t.copied)
        })
    }
    async function openMessage(item: LogObservation) {
        const id = selected, version = ++messageVersion
        message = item; messageBody = null
        try {
            const result = await networkLog<LogBodyPage>({ op: 'body', id, side: item.side, offset: item.offset, length: item.length, hex: item.kind !== 'text' && item.kind !== 'close' })
            if (mounted && id === selected && version === messageVersion && tab === 'events') messageBody = result
        } catch (value) { error = reason(value) }
    }
    onMount(() => {
        mounted = true
        void networkLog<NetworkLogSettings>({ op: 'settings' }).then(value => { if (mounted) settings = value; return refresh() }).catch(value => { error = reason(value) })
        const interval = setInterval(() => { if (!document.hidden && !busy) void refresh() }, 1500)
        return () => { mounted = false; listVersion++; detailVersion++; clearInterval(interval); clearTimeout(searchTimer); clearTimeout(noticeTimer) }
    })
</script>

<div class="network-log min-w-0 pb-5">
    <SettingGroup title={t.title} description={t.notice}>
        {#snippet actions()}
            {#if settings}<span class="flex items-center gap-2 text-xs text-subtext"><span class="h-1.5 w-1.5 rounded-full" class:bg-green-500={settings.enabled} class:bg-textcolor2={!settings.enabled}></span>{settings.enabled ? t.on : t.off}</span>{/if}
        {/snippet}
        {#if settings}
            <SettingRow label={t.recording} help={t.recordingHelp} inline><SettingToggle label={t.recording} checked={settings.enabled} disabled={busy} onchange={enabled => configure({ enabled })} /></SettingRow>
            <SettingRow label={t.retention} help={t.retentionHelp} inline><SettingToggle label={t.retention} checked={settings.retain} disabled={busy} onchange={retain => configure({ retain })} /></SettingRow>
            <details class="advanced">
                <summary class="flex cursor-pointer items-center justify-between px-4 py-3 text-sm">{t.advanced}<ChevronDown size={15}/></summary>
                <div class="divide-y divide-darkborderc/55 border-t border-darkborderc/55">
                    <SettingRow label={t.hideAuth} help={t.hideAuthHelp} inline>
                        <SettingToggle label={t.hideAuth} checked={settings.hideAuth} disabled={busy} onchange={hideAuth => configure({ hideAuth })}/>
                    </SettingRow>
                    <SettingRow label={t.captureBinary} help={t.captureBinaryHelp} inline>
                        <SettingToggle label={t.captureBinary} checked={settings.captureBinary} disabled={busy} onchange={captureBinary => configure({ captureBinary })}/>
                    </SettingRow>
                    <SettingRow label={t.recentCount} labelFor="network-recent-count"><select id="network-recent-count" class="risu-select" value={settings.recentCount} disabled={busy} onchange={event => configure({ recentCount: Number(event.currentTarget.value) })}>{#each [100, 200, 500, 1000] as count}<option value={count}>{count.toLocaleString()}</option>{/each}</select></SettingRow>
                    {#each [{ key: 'recentMib', label: t.recentBudget }, { key: 'retainedMib', label: t.retainedBudget }, { key: 'bufferMib', label: t.bufferBudget }] as option}
                        <SettingRow label={option.label} labelFor={'network-' + option.key}>
                            <select id={'network-' + option.key} class="risu-select" value={settings[option.key as 'recentMib' | 'retainedMib' | 'bufferMib']} disabled={busy} onchange={event => configure({ [option.key]: Number(event.currentTarget.value) })}>{#each option.key === 'bufferMib' ? [4, 8, 16, 32, 64, 128] : budgets as budget}<option value={budget}>{logSize(budget * 1024 * 1024)}</option>{/each}</select>
                        </SettingRow>
                    {/each}
                </div>
            </details>
        {:else}<p class="p-4 text-sm text-subtext">{t.loading}</p>{/if}
    </SettingGroup>
    {#if error}<p class="mt-3 break-words text-sm text-draculared" role="alert">{error}</p>{/if}
    {#if listing?.warning}<p class="mt-3 text-sm text-draculared" role="status">{reason(listing.warning)}</p>{/if}
    <p class="mt-2 min-h-4 text-xs text-subtext" aria-live="polite">{notice}</p>
    <div class="mt-2 flex flex-wrap items-center justify-between gap-2">
        <div class="flex gap-1 rounded-md bg-darkbg p-1" aria-label={t.title}>
            <button class:scope-active={!retained} onclick={() => { retained = false; filter() }}>{t.recent}</button>
            <button class:scope-active={retained} onclick={() => { retained = true; filter() }}>{t.retained}</button>
        </div>
        <button class="icon-button" aria-label={t.refresh} onclick={() => refresh(true)}><RefreshCw size={15}/></button>
    </div>
    <div class="mt-3 flex flex-wrap gap-2">
        <label class="search-box flex min-w-0 flex-1 items-center gap-2 rounded-md border border-darkborderc bg-darkbg px-3"><Search size={15} class="shrink-0 text-subtext"/><input class="min-w-0 flex-1 border-0 bg-transparent py-2 text-sm outline-none" aria-label={t.search} placeholder={t.search} bind:value={search} oninput={searchChanged}/></label>
        <select class="risu-select" aria-label={t.status} bind:value={status} onchange={filter}><option value="all">{t.all}</option><option value="error">{t.errors}</option><option value="active">{t.active}</option></select>
    </div>
    <div class="mt-3 overflow-hidden rounded-lg border border-darkborderc bg-darkbg">
        {#each groupNetworkLogItems(listing?.items ?? []) as group (group[0].operationId ?? group[0].id)}
            <div class="request-group border-b border-darkborderc/50 last:border-b-0" class:operation-group={!!group[0].operationId}>
            {#if group[0].operationId}<div class="px-3 pt-2 text-xs text-subtext">{t.operation} <span class="font-mono break-all">{group[0].operationId}</span></div>{/if}
            {#each group as item (item.id)}
            <button class="request-row w-full border-b border-darkborderc/50 px-3 py-3 text-left last:border-b-0" class:request-selected={selected === item.id} onclick={() => select(item.id)} aria-pressed={selected === item.id}>
                <span class="row-main flex min-w-0 items-center gap-2"><span class="method shrink-0 text-[11px] font-semibold">{item.method}</span><span class="min-w-0 flex-1 truncate text-sm">{item.url}</span><span class="shrink-0 font-mono text-xs" class:text-draculared={item.state === 'error' || (item.status ?? 0) >= 400}>{item.status ?? (item.state === 'active' ? '…' : '×')}</span>{#if item.retained}<Bookmark size={12} class="shrink-0 text-subtext"/>{/if}</span>
                <span class="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-subtext"><time>{new Date(item.startedAt).toLocaleTimeString()}</time><span>{item.source}</span><span>{item.elapsedMs.toLocaleString()} ms</span><span>{logSize(item.capturedBytes)}</span>{#if item.streamKind}<span>{item.streamKind === 'sse' ? 'SSE' : 'WebSocket'}</span>{/if}{#if item.incomplete}<span>{t.partial}</span>{/if}</span>
            </button>
            {/each}
            </div>
        {:else}<div class="px-4 py-10 text-center text-sm text-subtext">{t.empty}</div>{/each}
    </div>
    <div class="mt-2 flex flex-wrap items-center justify-between gap-2 text-xs text-subtext">
        <span>{t.storage} {logSize((retained ? listing?.retainedBytes : listing?.recentBytes) ?? 0)}{#if settings} / {logSize((retained ? settings.retainedMib : settings.recentMib) * 1024 * 1024)}{/if}</span>
        {#if offset > 0 || listing?.hasMore}<div class="flex gap-2"><button disabled={offset === 0} onclick={() => { offset = Math.max(0, offset - 50); void refresh() }}>{t.previous}</button><button disabled={!listing?.hasMore} onclick={() => { offset += 50; void refresh() }}>{t.next}</button></div>{/if}
    </div>
    {#if detail}
        <section class="mt-5 min-w-0 overflow-hidden rounded-lg border border-darkborderc bg-darkbg" aria-label={detail.request.url}>
            <div class="border-b border-darkborderc px-4 py-3">
                <div class="flex flex-wrap items-center justify-between gap-2"><div class="flex items-center gap-2 text-sm"><span class="font-semibold">{detail.request.method}</span><span>{detail.response.status ?? ''}</span></div><div class="flex flex-wrap gap-1">
                    {#if !detail.retained}<button disabled={busy} onclick={() => action(async () => { await networkLog({ op: 'retain', id: selected }); await refresh(true) })}><Bookmark size={14}/>{t.keep}</button>{/if}
                    <button disabled={busy} onclick={() => action(() => downloadNetworkLog(selected))}><Download size={14}/>{t.download}</button>
                    <button disabled={busy || detail.state === 'active'} aria-label={t.delete} onclick={() => action(async () => { await networkLog({ op: 'delete', id: selected }); selected = ''; detail = null; body = null; await refresh() })}><Trash2 size={14}/></button>
                </div></div>
                <p class="mt-2 break-all font-mono text-xs text-subtext">{detail.request.url}</p>
                {#if detail.response.url && detail.response.url !== detail.request.url}<p class="mt-1 break-all font-mono text-xs text-subtext">→ {detail.response.url}</p>{/if}
                {#if detail.captureError}<p class="mt-2 text-xs text-draculared">{reason(detail.captureError)}</p>{/if}
            </div>
            <div class="flex overflow-x-auto border-b border-darkborderc px-2 [scrollbar-width:none]" role="tablist" aria-label={t.title}>
                {#each [{ key: 'headers', label: t.headers }, { key: 'request', label: t.requestBody }, { key: 'response', label: t.responseBody }, ...(detail.streamKind ? [{ key: 'events', label: detail.streamKind === 'sse' ? t.events : t.messages }] : [])] as item}
                    <button class="detail-tab shrink-0" class:tab-active={tab === item.key} role="tab" aria-selected={tab === item.key} onclick={() => selectTab(item.key as typeof tab)}>{item.label}</button>
                {/each}
            </div>
            {#if tab === 'headers'}
                <div class="space-y-5 p-4">
                    {#each [{ title: t.requestHeaders, headers: detail.request.headers }, { title: t.responseHeaders, headers: detail.response.headers }] as section}
                        <div><h3 class="mb-2 text-sm font-semibold">{section.title}</h3><dl class="headers space-y-2 font-mono text-xs">{#each section.headers as header}<div class="header-row"><dt class="break-all text-subtext">{header.name}</dt><dd class="min-w-0 whitespace-pre-wrap break-all">{header.value}{header.encoding ? ' (' + header.encoding + ')' : ''}</dd></div>{:else}<p class="text-subtext">{t.noHeaders}</p>{/each}</dl></div>
                    {/each}
                </div>
            {:else if body}
                <div class="flex flex-wrap items-center justify-between gap-2 border-b border-darkborderc/50 px-3 py-2 text-xs text-subtext">
                    <span>{logSize(body.offset)} – {logSize(body.nextOffset)} / {logSize(body.total)}</span>
                    <div class="flex flex-wrap gap-1">
                        {#if prettyText !== null && tab !== 'events'}<button onclick={() => { pretty = !pretty }}>{pretty ? t.raw : t.formatted}</button>{/if}
                        {#if tab !== 'events'}<button disabled={busy} onclick={() => copy(selectedSide)}><Copy size={13}/>{t.copy}</button>{/if}
                    </div>
                </div>
                {#if tab === 'events' && detail.streamKind === 'sse'}
                    <div class="divide-y divide-darkborderc/50">{#each sseEvents as event, index}<details class="event-row px-4 py-2"><summary class="flex flex-wrap cursor-pointer gap-2 text-xs"><span class="text-subtext">#{sequence + index}</span>{#if arrivals[index] != null}<time class="text-subtext" title={new Date(detail.startedAt + arrivals[index]!).toISOString()}>{new Date(detail.startedAt + arrivals[index]!).toLocaleTimeString(undefined, { hour12: false })}.{String((detail.startedAt + arrivals[index]!) % 1000).padStart(3, '0')}</time>{/if}<span class="font-mono">{event.event}</span>{#if event.id}<span>{event.id}</span>{/if}{#if !event.complete || event.partial}<span class="text-subtext">{t.fragment}</span>{/if}</summary><pre class="code mt-2">{event.data || event.raw}</pre><details class="mt-2 text-xs text-subtext"><summary class="cursor-pointer">{t.raw}</summary><pre class="code mt-2">{event.raw}</pre></details></details>{:else}<p class="p-4 text-sm text-subtext">{t.missingEvents}</p>{/each}</div>
                {:else if tab === 'events' && detail.streamKind === 'websocket'}
                    <div class="divide-y divide-darkborderc/50">{#each observations as item}<button class="flex w-full justify-between rounded-none px-4 py-2 text-xs" onclick={() => openMessage(item)}><span class="flex gap-3"><span class="text-subtext">{item.elapsedMs.toLocaleString()} ms</span><span>{item.side === 'request' ? t.sent : t.received}</span><span class="font-mono">{item.kind}</span></span><span>{logSize(item.length)}</span></button>{:else}<p class="p-4 text-sm text-subtext">{t.missingEvents}</p>{/each}</div>
                    {#if message}<div class="border-t border-darkborderc p-4"><h4 class="mb-2 text-xs text-subtext">{message.kind} · {logSize(message.length)}</h4><pre class="code">{messageBody?.text ?? t.loading}</pre></div>{/if}
                {:else}
                    {#if detail[selectedSide].body.truncated && detail.captureError !== 'binary-skipped'}<p class="px-4 pt-3 text-xs text-draculared">{t.bodyLimited}</p>{/if}
                    <pre class="code p-4">{pretty && prettyText !== null ? prettyText : (body.text || t.noBody)}</pre>
                {/if}
                {#if bodyOffset > 0 || body.nextOffset < body.total}<div class="flex justify-end gap-2 border-t border-darkborderc px-3 py-2"><button disabled={pageHistory.length === 0} onclick={() => bodyPage(false)}>{t.previous}</button><button disabled={body.nextOffset >= body.total} onclick={() => bodyPage(true)}>{t.next}</button></div>{/if}
            {:else}<p class="p-4 text-sm text-subtext">{t.loading}</p>{/if}
        </section>
    {/if}
</div>

<style>
    .network-log button { display: inline-flex; align-items: center; justify-content: center; gap: 0.4rem; padding: 0.4rem 0.65rem; border-radius: 0.375rem; font-size: 0.75rem; }
    .network-log button:hover:not(:disabled) { background: var(--risu-theme-darkbutton); }
    .network-log button:disabled { opacity: 0.4; cursor: not-allowed; }
    .network-log select { max-width: 100%; min-width: 6.5rem; padding: 0.4rem 2.25rem 0.4rem 0.6rem; border: 1px solid var(--risu-theme-darkborderc); border-radius: 0.375rem; background-color: var(--risu-theme-darkbg); color: var(--risu-theme-textcolor); font-size: 0.8125rem; }
    .network-log .scope-active { background: var(--risu-theme-darkbutton); color: var(--risu-theme-textcolor); }
    .network-log .request-row { display: block; border-radius: 0; padding: 0.75rem; }
    /* A shadow, not a border, so grouped rows keep their alignment and a selected row draws one marker. */
    .network-log .operation-group { box-shadow: inset 2px 0 var(--risu-theme-selected); }
    .network-log .request-selected { background: color-mix(in srgb, var(--risu-theme-selected) 16%, var(--risu-theme-darkbg)); box-shadow: inset 2px 0 var(--risu-theme-textcolor2); }
    .network-log .detail-tab { border-radius: 0; padding: 0.75rem 0.5rem; border-bottom: 2px solid transparent; color: var(--color-subtext); }
    .network-log .tab-active { border-bottom-color: var(--risu-theme-textcolor); color: var(--risu-theme-textcolor); }
    .header-row { display: grid; grid-template-columns: minmax(6rem, 32%) minmax(0, 1fr); gap: 0.75rem; }
    .code { max-height: 32rem; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 0.75rem; line-height: 1.65; }
    @media (max-width: 420px) { .header-row { grid-template-columns: minmax(0, 1fr); gap: 0.15rem; } }
    @media (pointer: coarse) { .network-log button, .network-log select, .advanced > summary, .event-row > summary { min-height: 44px; } .network-log input, .network-log select { font-size: 16px; } }
</style>
