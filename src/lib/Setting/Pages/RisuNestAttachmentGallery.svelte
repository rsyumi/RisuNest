<script lang="ts">
    import { onMount, tick } from 'svelte'
    import { MapPinIcon, ArrowRightIcon, RefreshCwIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { isTauri } from 'src/ts/platform'
    import { DBState } from 'src/ts/stores.svelte'
    import { alertActionConfirm, alertError, alertToast } from 'src/ts/alert'
    import GalleryGrid from '../../Gallery/GalleryGrid.svelte'
    import GalleryViewer from '../../Gallery/GalleryViewer.svelte'
    import GalleryToolbar from '../../Gallery/GalleryToolbar.svelte'
    import SettingNotice from '../RisuNest/SettingNotice.svelte'
    import SettingProgress from '../RisuNest/SettingProgress.svelte'
    import '../../Gallery/gallery.css'
    import { galleryText, galleryFormat as fmt } from 'src/ts/gallery/strings'
    import { InlayGalleryPaging } from 'src/ts/gallery/inlayGalleryPaging'
    import { retainGallerySelection, readGallerySelectionRange } from 'src/ts/gallery/inlayGalleryState'
    import { gallerySize } from 'src/ts/gallery/preferences'
    import { galleryBytes, type GalleryItem } from 'src/ts/gallery/types'
    import { downloadGallery, galleryArchiveDate } from 'src/ts/gallery/galleryDownloads'
    import { nativeFileOperation } from 'src/ts/storage/nativeFileJobManager'
    import { contentExportBusyMessage } from 'src/ts/storage/fileOperationErrorPresentation'
    import { readGalleryOrigin, jumpToGalleryLocation, type GalleryOrigin } from 'src/ts/gallery/inlayGalleryNavigation'
    import {
        readInlayGalleryPage, readInlayGalleryKeys, inlayGalleryIndex, missingGalleryKeys, galleryRevision,
        readGalleryLocations, startGalleryScan, readGalleryScan, readGalleryScanResult, cancelGalleryScan, deleteGalleryKeys,
        galleryWindow, deleteUnusedGallerySelection, isGalleryRevisionConflict, readGalleryMetadata, galleryUsageCount,
        type InlayGalleryQuery, type GalleryScanStatus, type GalleryLocations, type GalleryLocation,
    } from 'src/ts/gallery/inlayGallery'
    import { flushPendingData } from 'src/ts/storage/persistentDataRuntime.svelte'
    import { subscribeLocalPersistentRevision } from 'src/ts/storage/persistentRevisionEvents'
    import { getInlayEncodeOptions } from 'src/ts/process/files/inlays'
    import { selectInlayOptimizationTargets, emptyInlayOptimizationProgress, inlayOptimizationWarnings } from 'src/ts/process/files/inlayOptimizationJob'
    import { inlayOptimizationProgressMessage, inlayOptimizationResultMessage } from 'src/ts/process/files/inlayOptimizationMessages'
    import { createStoredInlayOptimizationDeps, readInlayOptimizationEnvironment } from 'src/ts/process/files/inlayOptimizationRuntime'
    import { inlayOptimizationController as optimization } from 'src/ts/process/files/inlayOptimizationController.svelte'

    const text = galleryText()
    let query = $state<InlayGalleryQuery>({ type: 'all', sort: 'newest', usage: 'all', offset: 0, limit: 100 })
    let items = $state(new Map<number, GalleryItem>())
    let total = $state(0)
    let totalBytes = $state(0)
    let counts = $state({ all: 0, image: 0, video: 0, audio: 0 })
    let revision = $state(-1)
    let scan = $state<GalleryScanStatus | null>(null)
    let completedScan = $state<GalleryScanStatus | null>(null)
    let selected = $state(new Set<string>())
    let selecting = $state(false)
    let anchor = -1
    let selectionGeneration = 0
    let loading = $state(true)
    let error = $state(false)
    let busy = $state(false)
    let viewer = $state<GalleryItem | null>(null)
    let viewerIndex = $state(0)
    let viewerOpener: { key: string; index: number } | null = null
    let neighbors = $state(new Map<number, GalleryItem>())
    let locations = $state<GalleryLocations | null>(null)
    let origin = $state<GalleryOrigin | null>(null)
    let grid: GalleryGrid | undefined = $state()
    let scanRunning = $state(false)
    let scanInitialized = $state(!isTauri)
    let optimizing = $state(false)
    let optimizeCancel = false
    let optimizeProgress = $state(emptyInlayOptimizationProgress())
    let resultText = $state('')
    let downloadRunning = $state(false)
    const waitingForSync = $derived(downloadRunning && $nativeFileOperation?.waitingForSync === true)
    let downloadArchive = $state(true)
    let downloadProgress = $state({ done: 0, total: 0 })
    let downloadResult = $state('')
    let downloadAbort: AbortController | undefined
    let mounted = false
    let generation = 0
    let viewerGeneration = 0
    let refreshTimer: ReturnType<typeof setTimeout> | undefined
    let revisionPollRunning = false
    let pendingPage: Promise<void> | null = null
    let pendingLoad: Promise<void> | null = null
    let refreshing: Promise<void> | null = null
    let refreshViewerPending = false
    const stale = $derived(completedScan !== null && completedScan.revision !== revision)
    const shownOptimizationProgress = $derived(optimizing && optimization.running ? { ...optimizeProgress, scanned: optimizeProgress.scanned + optimization.progress.scanned } : optimizeProgress)
    const disabled = $derived(busy || optimizing || optimization.running)

    function report(cause: unknown) { alertError(contentExportBusyMessage(cause) ?? (cause instanceof Error ? cause : String((cause as { message?: string })?.message ?? cause))) }
    const paging = new InlayGalleryPaging(readInlayGalleryPage, (page, window) => {
        if (!mounted) return
        revision = page.revision
        total = page.total
        totalBytes = page.totalBytes
        counts = page.counts
        items = window
        error = false
        if (total && query.offset >= total) requestWindow(Math.max(0, total - query.limit), query.limit)
    })
    function load(refreshViewer = false, force = false): Promise<void> {
        if (force) generation++
        refreshViewerPending ||= refreshViewer
        const completion = paging.request({ ...query }, force)
        if (pendingPage === completion) return pendingLoad!
        pendingPage = completion
        loading = true
        pendingLoad = (async () => {
            try {
                await completion
                if (!mounted || pendingPage !== completion) return
                const token = generation
                error = false
                const updateViewer = refreshViewerPending
                if (updateViewer && viewer) {
                    const index = await inlayGalleryIndex({ ...query }, viewer.key)
                    if (!mounted || token !== generation || pendingPage !== completion) return
                    if (!total) await closeViewer()
                    else await openIndex(index ?? Math.min(viewerIndex, total - 1))
                }
                if (pendingPage === completion && token === generation) refreshViewerPending = false
            } catch (cause) { if (pendingPage === completion) error = true }
            finally {
                if (pendingPage === completion) { pendingPage = null; pendingLoad = null; loading = false }
            }
        })()
        return pendingLoad
    }
    function refresh(): Promise<void> {
        if (disabled) return Promise.resolve()
        if (refreshing) return refreshing
        refreshing = (async () => {
            try {
                if (selected.size) {
                    const missing = await missingGalleryKeys(selected)
                    // Applied to the current selection, so toggles made during the check stay.
                    if (missing.size) selected = new Set([...selected].filter(key => !missing.has(key)))
                }
                await load(true, true)
            } catch (cause) { report(cause) }
        })().finally(() => { refreshing = null })
        return refreshing
    }
    function scheduleRefresh(observedRevision?: number) {
        if (observedRevision !== undefined) paging.observeRevision(observedRevision)
        if (refreshTimer || disabled) return
        refreshTimer = setTimeout(() => { refreshTimer = undefined; if (mounted) void refresh() }, 250)
    }
    function requestWindow(offset: number, limit: number) {
        const count = Math.max(1, limit)
        if (query.offset === offset && query.limit === count) return
        query.offset = offset
        query.limit = count
        void load()
    }
    function clearSelection() { selectionGeneration++; selected = new Set(); selecting = false; anchor = -1 }
    function changeFilter() { query.offset = 0; clearSelection(); items = new Map(); void load(false, true); void grid?.scrollToIndex(0).catch(report) }
    async function selectAll() {
        const token = generation
        const selectionToken = ++selectionGeneration
        try {
            const keys = await readInlayGalleryKeys({ ...query })
            if (mounted && token === generation && selectionToken === selectionGeneration) selected = new Set(keys)
        } catch (cause) { report(cause) }
    }
    async function toggle(item: GalleryItem, index: number, range: boolean) {
        try {
            const token = generation
            const selectionToken = ++selectionGeneration
            const currentQuery = { ...query }
            const keys = range && anchor >= 0 ? await readGallerySelectionRange(anchor, index,
                (offset, limit) => readInlayGalleryPage({ ...currentQuery, offset, limit })) : null
            if (!mounted || token !== generation || selectionToken !== selectionGeneration) return
            const next = new Set(selected)
            if (keys) {
                for (const key of keys) next.add(key)
            } else if (next.has(item.key)) next.delete(item.key)
            else next.add(item.key)
            selected = next
            anchor = index
        } catch (cause) { report(cause) }
    }
    function openViewer(item: GalleryItem, index: number) {
        viewerOpener = { key: item.key, index }
        void openIndex(index)
    }
    async function openIndex(index: number) {
        if (index < 0 || index >= total) return
        const token = ++viewerGeneration
        const offset = Math.max(0, index - 1)
        try {
            const page = await readInlayGalleryPage({ ...query, offset, limit: 3 })
            if (!mounted || token !== viewerGeneration) return
            neighbors = galleryWindow(page.items, offset)
            viewer = neighbors.get(index) ?? null
            viewerIndex = index
            locations = null
            origin = null
            if (!viewer) return
            const current = viewer
            const [made, used] = await Promise.all([readGalleryOrigin(current), isTauri ? readGalleryLocations(current.key) : null])
            if (token === viewerGeneration) { origin = made; locations = used }
        } catch (cause) { report(cause) }
    }
    async function closeViewer() {
        const token = ++viewerGeneration
        const opener = viewerOpener
        viewerOpener = null
        viewer = null
        neighbors = new Map()
        locations = null
        origin = null
        await tick()
        if (!mounted || !opener || !total) return
        try {
            const index = await inlayGalleryIndex(query, opener.key)
            if (mounted && token === viewerGeneration && !viewer) {
                await grid?.focusItem(index ?? Math.min(opener.index, total - 1))
            }
        } catch (cause) { report(cause) }
    }
    async function followScan(initial: GalleryScanStatus): Promise<boolean> {
        scanRunning = true
        try {
            let status = initial
            scan = status
            while (status.state === 'running' && mounted) {
                await new Promise(resolve => setTimeout(resolve, 250))
                const next = await readGalleryScan()
                if (!next || next.id !== status.id) return false
                status = next
                scan = status
            }
            if (!mounted) return false
            if (status.state === 'failed') throw new Error(status.error ?? text.loadFailed)
            if (status.state === 'complete') completedScan = status
            await load(true, true)
            return status.state === 'complete'
        } finally { scanRunning = false }
    }
    async function initializeScan() {
        if (!isTauri) return
        try {
            const status = await readGalleryScan()
            const result = status?.state === 'complete' ? status : await readGalleryScanResult()
            if (!mounted) return
            scan = status
            completedScan = result
            scanInitialized = true
            if (status?.state === 'running') await followScan(status)
        } catch (cause) { if (mounted) report(cause) }
        finally { scanInitialized = true }
    }
    async function runScan(): Promise<boolean> {
        if (!isTauri || !scanInitialized || scanRunning) return false
        scanRunning = true
        try {
            await flushPendingData('gallery-usage-scan')
            return await followScan(await startGalleryScan(await galleryRevision()))
        } catch (cause) { report(cause); return false }
        finally { scanRunning = false }
    }
    async function confirmDelete(keys: string[], dropped = 0) {
        const uses = isTauri && completedScan ? await galleryUsageCount(keys) : 0
        const description = [keys.length === 1 ? text.deleteAttachmentOne : fmt(text.deleteAttachmentMany, keys.length),
            completedScan ? fmt(text.usedLocations, uses) : '', dropped ? fmt(text.recheckDropped, dropped) : ''].filter(Boolean).join('\n\n')
        return alertActionConfirm({ title: text.deleteAttachment, description, actionLabel: text.remove, cancelLabel: text.cancel })
    }
    async function remove(keys: string[]) {
        if (disabled || !keys.length) return
        busy = true
        try {
            if (isTauri && query.usage === 'unused') {
                const targets = new Set(keys)
                await deleteUnusedGallerySelection(targets, {
                    flush: () => flushPendingData('gallery-delete-unused'), revision: galleryRevision,
                    scanRevision: () => completedScan?.revision ?? null,
                    rescan: runScan, unusedKeys: () => readInlayGalleryKeys({ ...query, type: 'all', usage: 'unused', characterId: undefined }),
                    confirm: confirmDelete, remove: (keys, revision) => deleteGalleryKeys(keys, revision, completedScan?.id),
                    selection: value => { selected = retainGallerySelection(selected, targets, value) }, isConflict: isGalleryRevisionConflict,
                    allDropped: count => alertToast(fmt(text.recheckDropped, count)),
                })
            } else {
                if (!await confirmDelete(keys)) return
                await flushPendingData('gallery-delete')
                await deleteGalleryKeys(keys, await galleryRevision())
                const removed = new Set(keys)
                selected = new Set([...selected].filter(key => !removed.has(key)))
            }
        } catch (cause) { report(cause) }
        finally { busy = false; await refresh() }
    }
    async function download(keys: string[], single?: GalleryItem) {
        if (downloadRunning) return
        downloadRunning = true
        downloadArchive = !single
        downloadResult = ''
        downloadProgress = { done: 0, total: keys.length }
        downloadAbort = new AbortController()
        try {
            const result = await downloadGallery({ kind: 'inlay', keys, archive: !single, item: single,
                suggestedName: single?.name ?? `${text.attachmentTab} ${galleryArchiveDate(new Date())}.zip` },
                { signal: downloadAbort.signal, onProgress: value => { downloadProgress = value } })
            if (!result.cancelled) {
                if (single) alertToast(text.fileSaved)
                else downloadResult = result.skipped ? fmt(text.zipSkipped, result.skipped) : text.zipSaved
            }
        } catch (cause) { report(cause) }
        finally { downloadRunning = false }
    }
    async function optimize() {
        if (disabled || !selected.size) return
        optimizing = true
        optimizeCancel = false
        resultText = ''
        try {
            const options = getInlayEncodeOptions()
            const keys = [...selected]
            const targets: string[] = []
            let bytes = 0
            for (let offset = 0; offset < keys.length; offset += 128) {
                for (const item of selectInlayOptimizationTargets(await readGalleryMetadata(keys.slice(offset, offset + 128)), options)) {
                    targets.push(item.key); bytes += item.size
                }
            }
            if (!targets.length) { resultText = language.risuNest.inlay.optimizeNone; return }
            const environment = await readInlayOptimizationEnvironment()
            const strings = language.risuNest.inlay
            const warning = inlayOptimizationWarnings({ storedFormat: options.format, ...environment })
                .map(value => value === 'format' ? strings.optimizeFormatNotice : value === 'sync' ? strings.optimizeSyncNotice : strings.optimizeRemoteNotice)
            const description = [strings.optimizeConfirm.replace('{count}', String(targets.length)).replace('{size}', galleryBytes(bytes)), ...warning].join('\n\n')
            if (!await alertActionConfirm({ title: text.optimize, description, actionLabel: text.optimize, cancelLabel: text.cancel })) return
            optimizeProgress = emptyInlayOptimizationProgress(targets.length)
            const accumulated = emptyInlayOptimizationProgress(targets.length)
            for (let offset = 0; offset < targets.length && !optimizeCancel; offset += 128) {
                const batch = await readGalleryMetadata(targets.slice(offset, offset + 128))
                const completed = await optimization.start(batch, options, createStoredInlayOptimizationDeps())
                if (completed) {
                    for (const field of ['scanned', 'converted', 'skipped', 'failed', 'beforeBytes', 'afterBytes'] as const) accumulated[field] += completed[field]
                    optimizeProgress = { ...accumulated }
                }
            }
            resultText = inlayOptimizationResultMessage(optimizeProgress)
        } catch (cause) { report(cause) }
        finally { optimizing = false; await refresh() }
    }
    function locationLabel(location: GalleryLocation): string {
        const character = location.characterName ?? DBState.db.characters.find(value => value.chaId === location.characterId)?.name
        if (character) return [character, location.conversationName].filter(Boolean).join(' · ')
        if (location.kind === 'settings') return text.settingsLocation
        if (location.kind === 'pending') return text.pendingLocation
        if (location.kind === 'plugin') return text.pluginData
        if (location.kind === 'character') return text.characterSettings
        return text.archived
    }
    async function jump(location: GalleryLocation, origin = false) {
        try { await jumpToGalleryLocation(location, () => { void closeViewer() }, origin) }
        catch (cause) { report(cause) }
    }
    onMount(() => {
        mounted = true
        void load()
        void initializeScan()
        const unsubscribe = subscribeLocalPersistentRevision(scheduleRefresh)
        const timer = setInterval(async () => {
            if (disabled || revisionPollRunning) return
            revisionPollRunning = true
            try { const current = await galleryRevision(); if (current !== revision) scheduleRefresh(current) } catch { /* The next refresh reports a read failure. */ }
            finally { revisionPollRunning = false }
        }, 1000)
        return () => { mounted = false; generation++; viewerGeneration++; paging.dispose(); unsubscribe(); clearInterval(timer); clearTimeout(refreshTimer); downloadAbort?.abort(); if (scanRunning) void cancelGalleryScan().catch(() => {}) }
    })
</script>

<div class="attachment-gallery" data-attachment-gallery>
    <div class="attachment-heading">
        <h2 class="text-lg font-semibold">{text.attachments}</h2>
        <div class="attachment-heading-actions">
            {#if isTauri}<button class="g-button small" disabled={disabled || scanRunning || !scanInitialized} onclick={() => runScan()}><MapPinIcon size={14} />{text.scanUsage}</button>{/if}
            <button class="g-button small quiet" disabled={disabled || loading} onclick={refresh}><RefreshCwIcon size={14} />{text.refresh}</button>
        </div>
    </div>
    <p class="attachment-summary text-sm text-subtext">{language.risuNest.inlay.inventoryTotal.replace('{count}', String(total)).replace('{size}', galleryBytes(totalBytes))}</p>
    <div class="g-panel attachment-panel">
    <div class="attachment-toolbar" data-gallery-toolbar>
        {#if selected.size}
            <GalleryToolbar selectedCount={selected.size} {total} onselectall={selectAll} onclear={clearSelection}>
                {#if isTauri}<button class="g-button small" disabled={disabled || downloadRunning} onclick={() => download([...selected])}>{text.download}</button>{/if}
                <button class="g-button small" disabled={disabled} onclick={optimize}>{text.optimize}</button>
                <button class="g-button small danger" disabled={disabled} onclick={() => remove([...selected])}>{text.remove}</button>
            </GalleryToolbar>
        {:else}
            <div class="g-toolbar attachment-filters">
                <div class="g-row attachment-types">
                {#each ['all', 'image', 'video', 'audio'] as const as type}<button class="g-chip" class:active={query.type === type} onclick={() => { query.type = type; changeFilter() }}>{text[type]} <span>{counts[type]}</span></button>{/each}
                </div>
                <div class="g-row attachment-controls">
                <select class="risu-select h-[1.875rem] max-w-full rounded-md border border-darkborderc bg-darkbg pl-2.5 text-[13px] text-textcolor" aria-label={text.sort} bind:value={query.sort} onchange={changeFilter}><option value="newest">{text.newest}</option><option value="oldest">{text.oldest}</option><option value="largest">{text.largest}</option></select>
                <div class="g-segment"><button class:active={$gallerySize === 'normal'} onclick={() => gallerySize.set('normal')}>{text.normal}</button><button class:active={$gallerySize === 'large'} onclick={() => gallerySize.set('large')}>{text.large}</button></div>
                <button class="g-button small" class:primary={selecting} onclick={() => { selecting = !selecting }}>{text.select}</button>
                </div>
            </div>
        {/if}
    </div>
        {#if isTauri && completedScan}
            <div class="g-toolbar">
                <button class="g-chip" class:active={query.usage === 'all'} onclick={() => { query.usage = 'all'; changeFilter() }}>{text.all}</button>
                <button class="g-chip" class:active={query.usage === 'unused'} onclick={() => { query.usage = 'unused'; changeFilter() }}>{text.unused} {completedScan.counts.unused}</button>
                {#if completedScan.counts.unknown}<button class="g-chip" class:active={query.usage === 'unknown'} onclick={() => { query.usage = 'unknown'; changeFilter() }}>{text.unknown} {completedScan.counts.unknown}</button>{/if}
                <select class="risu-select h-[1.875rem] max-w-full rounded-md border border-darkborderc bg-darkbg pl-2.5 text-[13px] text-textcolor" aria-label={text.character} value={query.characterId ?? ''} onchange={event => { query.characterId = event.currentTarget.value || undefined; changeFilter() }}><option value="">{text.allCharacters}</option>{#each completedScan.characters as character}<option value={character.characterId}>{character.name ?? DBState.db.characters.find(value => value.chaId === character.characterId)?.name ?? character.characterId} ({character.count})</option>{/each}</select>
            </div>
        {/if}
        <div class="g-notices" aria-live="polite">
            {#if completedScan?.uncheckedArchives}<SettingNotice tone="info" text={fmt(text.uncheckedArchives, completedScan.uncheckedArchives)} />{/if}
            {#if completedScan?.uncheckedRecords}<SettingNotice tone="info" text={fmt(text.uncheckedRecords, completedScan.uncheckedRecords)} />{/if}
            {#if stale}<div class="g-row"><div class="g-grow"><SettingNotice tone="info" text={text.stale} /></div><button class="g-button quiet" disabled={scanRunning || !scanInitialized} onclick={() => runScan()}>{text.recheck}</button></div>{/if}
            {#if error}<SettingNotice tone="danger" role="alert" text={text.loadFailed} />{/if}
            {#if resultText}<SettingNotice tone="info" text={resultText} />{/if}
            {#if downloadResult}<SettingNotice tone="info" text={downloadResult} />{/if}
        </div>
        {#if scanRunning && scan}<SettingProgress label={text.scanTitle} detail={fmt(text.scanDetail, scan.completedConversations, scan.totalConversations)} fraction={scan.totalItems ? scan.completedItems / scan.totalItems : null}>{#snippet actions()}<button class="g-button" onclick={() => { void cancelGalleryScan().catch(report) }}>{text.cancel}</button>{/snippet}</SettingProgress>{/if}
        {#if optimizing}<SettingProgress label={text.optimize} detail={inlayOptimizationProgressMessage(shownOptimizationProgress)} fraction={shownOptimizationProgress.total ? shownOptimizationProgress.scanned / shownOptimizationProgress.total : null}>{#snippet actions()}<button class="g-button" onclick={() => { optimizeCancel = true; optimization.cancel() }}>{text.cancel}</button>{/snippet}</SettingProgress>{/if}
        {#if downloadRunning}<SettingProgress label={downloadArchive ? text.zipTitle : text.download} detail={waitingForSync ? language.risuNest.importDialog.waitingForSync : fmt(text.zipProgress, downloadProgress.done, downloadProgress.total)} fraction={!waitingForSync && downloadProgress.total ? downloadProgress.done / downloadProgress.total : null}>{#snippet actions()}<button class="g-button" onclick={() => downloadAbort?.abort()}>{text.cancel}</button>{/snippet}</SettingProgress>{/if}
    {#if !loading && !total}<p class="p-6 text-subtext">{query.type !== 'all' || query.usage !== 'all' || query.characterId ? text.emptyFiltered : text.empty}</p>{/if}
    <GalleryGrid bind:this={grid} {items} {total} size={$gallerySize} {selected} {selecting} onwindow={requestWindow} onopen={openViewer} ontoggle={toggle} />
    </div>
</div>
{#if viewer}
    <GalleryViewer item={viewer} index={viewerIndex} {total} previous={neighbors.get(viewerIndex - 1)} next={neighbors.get(viewerIndex + 1)} onclose={closeViewer} onnavigate={openIndex}>
        {#snippet actions()}
            <button class="g-button" disabled={downloadRunning} onclick={() => viewer && download([viewer.key], viewer)}>{text.download}</button>
            <button class="g-button danger" disabled={disabled} onclick={() => viewer && remove([viewer.key])}>{text.remove}</button>
        {/snippet}
        {#snippet infoRows()}
            {#if origin}
                <dt class="origin-label">{text.origin}</dt>
                <dd class="origin-value">
                    <span><b>{origin.characterName}</b>{#if origin.conversationName} · {origin.conversationName}{/if}{#if origin.archived}<span class="location-pill">{text.archived}</span>{:else if origin.trashed}<span class="location-pill">{text.trash}</span>{/if}</span>
                    {#if !origin.archived && !origin.trashed}<button class="g-button small" onclick={() => origin && jump(origin, true)}><ArrowRightIcon size={13} />{text.jump}</button>{/if}
                </dd>
            {/if}
        {/snippet}
        {#snippet infoExtra()}
            {#if isTauri}
                <section>
                    <h3 class="usage-title">
                        {text.usageLocations}
                        {#if locations?.locations.length}<span class="usage-count">{fmt(text.locationCount, locations.locations.length)}</span>{/if}
                        {#if locations?.state && !scanRunning}<button class="g-button small quiet usage-recheck" disabled={!scanInitialized} onclick={() => runScan()}>{text.recheck}</button>{/if}
                    </h3>
                    {#if scanRunning && scan}
                        <SettingProgress label={text.scanTitle} detail={fmt(text.scanDetail, scan.completedConversations, scan.totalConversations)} fraction={scan.totalItems ? scan.completedItems / scan.totalItems : null}>{#snippet actions()}<button class="g-button small" onclick={() => { void cancelGalleryScan().catch(report) }}>{text.cancel}</button>{/snippet}</SettingProgress>
                    {:else if !locations?.state}
                        <p class="usage-help">{text.findLocationsHelp}</p>
                        <button class="g-button small" disabled={!scanInitialized} onclick={() => runScan()}><MapPinIcon size={13} />{text.findUsage}</button>
                    {:else if locations.state === 'unknown'}
                        {#if locations.uncheckedArchives}<p class="usage-help">{fmt(text.uncheckedArchives, locations.uncheckedArchives)}</p>{/if}
                        {#if locations.uncheckedRecords}<p class="usage-help">{fmt(text.uncheckedRecords, locations.uncheckedRecords)}</p>{/if}
                    {:else if !locations.locations.length}
                        <p class="usage-help">{text.usageEmpty}</p>
                    {:else}
                        <div class="usage-locations">
                            {#each locations.locations as location}
                                {@const characterName = location.characterName ?? DBState.db.characters.find(value => value.chaId === location.characterId)?.name}
                                <div class="usage-location">
                                    <span class="location-who">{#if characterName}<b>{characterName}</b>{#if location.conversationName} · {location.conversationName}{/if}{:else}{locationLabel(location)}{/if}{#if location.archived}<span class="location-pill">{text.archived}</span>{:else if location.trashed}<span class="location-pill">{text.trash}</span>{/if}</span>
                                    {#if !location.archived && !location.trashed && location.kind === 'message'}<button class="g-button small" onclick={() => jump(location)}><ArrowRightIcon size={13} />{text.jump}</button>{:else}<span></span>{/if}
                                    {#if location.messageIndex !== undefined}<small>{fmt(text.messageNumber, location.messageIndex + 1)}</small>{:else if location.kind === 'character'}<small>{text.characterSettings}</small>{:else if location.kind === 'conversation'}<small>{text.conversationLocation}</small>{/if}
                                </div>
                            {/each}
                        </div>
                    {/if}
                </section>
            {/if}
        {/snippet}
    </GalleryViewer>
{/if}
<style>
    .attachment-gallery { margin-top: 1.5rem; }
    .attachment-heading { display: flex; align-items: center; justify-content: space-between; gap: .5rem; }
    .attachment-heading h2 { min-width: 0; }
    .attachment-heading-actions { display: flex; flex: none; gap: .25rem; align-items: center; }
    .attachment-summary { margin-top: .25rem; }
    .attachment-types { flex: 1 1 auto; }
    .attachment-panel { overflow: clip; margin-top: .75rem; }
    .attachment-toolbar { position: sticky; top: -1.5rem; z-index: 5; border-radius: 0; background: var(--risu-theme-darkbg); }
    .origin-label { color: var(--color-subtext); font-size: 13px; }
    .origin-value { display: flex; align-items: center; gap: .5rem; flex-wrap: wrap; margin: 0; overflow-wrap: anywhere; font-size: 13px; }
    .origin-value b, .location-who b { font-weight: 600; }
    .usage-title { display: flex; align-items: center; gap: .5rem; font-size: .9375rem; font-weight: 600; margin: 1.25rem 0 .5rem; }
    .usage-count { font-size: 13px; font-weight: 400; color: var(--color-subtext); }
    .usage-recheck { margin-left: auto; }
    .usage-help { font-size: 13px; color: var(--color-subtext); line-height: 1.5; margin: 0 0 .625rem; }
    .usage-location { display: grid; grid-template-columns: minmax(0,1fr) auto; align-items: center; gap: .125rem .75rem; padding: .5rem 0; border-top-width:1px; border-top-style:solid; border-top-color:color-mix(in srgb,var(--risu-theme-darkborderc) 55%,transparent); font-size: 13px; }
    .usage-location:first-child { border-top: 0; }
    .location-who { min-width: 0; overflow-wrap: anywhere; }
    .usage-location small { grid-column: 1; color: var(--color-subtext); font-size: 12px; }
    .location-pill { display: inline-block; padding: .05rem .45rem; border: 1px solid var(--risu-theme-darkborderc); border-radius: 999px; background: var(--risu-theme-bgcolor); font-size: 11px; font-weight: 600; color: var(--color-subtext); margin-left: .25rem; vertical-align: 1px; }
</style>
