<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { ChevronRight } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { alertCheckboxConfirm, alertError, alertNormal } from 'src/ts/alert'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SettingProgress from '../RisuNest/SettingProgress.svelte'
    import {
        applyNativeDataHealthRepair,
        cancelNativeDataHealthScan,
        deepScanNativeDataHealth,
        discardNativeDataHealthIntent,
        completeNativeDataHealthIntent,
        getNativeDataHealthResult,
        holdNativeDataHealthScan,
        listNativeDataHealthJournals,
        planNativeDataHealthRepair,
        previewNativeDataHealthRepair,
        scanNativeDataHealth,
        undoNativeDataHealthRepair,
    } from 'src/ts/storage/nativePersistentMaintenance'
    import {
        dataHealthReportFileName,
        formatDataHealthReport,
        repairChoicesByFinding,
        type DataHealthFinding,
        type DataHealthSeverity,
        type RepairCandidate,
    } from 'src/ts/storage/dataHealth'
    import {
        dataHealthOwnerKey,
        describeDataHealthFinding,
        type DataHealthResolvedNames,
    } from 'src/ts/storage/dataHealthPresentation'
    import { createDataHealthModel } from 'src/ts/storage/dataHealthModel'
    import { formatRisuNestStorageBytes } from 'src/ts/storage/risuNestStorageDashboard'
    import { downloadFile } from 'src/ts/globalApi.svelte'

    interface Props {
        /** Scrolls to the unused image cleanup, which is the only place files are deleted. */
        onOpenUnusedImages?: () => void
        /**
         * Opens the store before the first command. The ordinary settings screen runs behind a
         * library that is already open; the recovery shell is the one place that is not true.
         */
        prepare?: () => Promise<void>
        resolveNames?: (
            items: readonly DataHealthFinding[],
        ) => Promise<Map<string, DataHealthResolvedNames>>
    }

    let {
        onOpenUnusedImages,
        prepare,
        resolveNames,
    }: Props = $props()

    const strings = language.risuNest.dataHealth
    const model = createDataHealthModel({
        getResult: getNativeDataHealthResult,
        scan: scanNativeDataHealth,
        deepScan: deepScanNativeDataHealth,
        cancel: cancelNativeDataHealthScan,
        holdBackground: holdNativeDataHealthScan,
        planRepair: planNativeDataHealthRepair,
        previewRepair: previewNativeDataHealthRepair,
        applyRepair: (selection, snapshot, revision, scannedAt) => repair(
            revision, (expected) => applyNativeDataHealthRepair(selection, snapshot, expected, scannedAt),
        ),
        discardIntent: discardNativeDataHealthIntent,
        completeIntent: (finding, revision, scannedAt) => repair(
            revision, (expected) => completeNativeDataHealthIntent(finding, expected, scannedAt),
        ),
        listJournals: listNativeDataHealthJournals,
        undoRepair: (journalId, revision) => repair(
            revision, (expected) => undoNativeDataHealthRepair(journalId, expected),
        ),
    })
    async function repair<T extends { revision: number }>(
        revision: number,
        mutate: (expectedRevision: number) => Promise<T>,
    ): Promise<T> {
        if (prepare) return mutate(revision)
        const [{ runNativeDataHealthRepair }, { getPersistentDataRuntime }] = await Promise.all([
            import('src/ts/storage/nativeDataHealthRepair'),
            import('src/ts/storage/persistentDataRuntime.svelte'),
        ])
        return runNativeDataHealthRepair(revision, mutate, getPersistentDataRuntime())
    }
    let view = $state(model.snapshot())
    let resolvedNames = $state(new Map<string, DataHealthResolvedNames>())
    let includeNames = $state(false)
    let keepSnapshot = $state(false)
    let namesGeneration = 0

    const severityLabels: Record<DataHealthSeverity, string> = {
        blocking: strings.severityBlocking,
        degraded: strings.severityDegraded,
        informational: strings.severityInformational,
    }
    const severityHelp: Record<DataHealthSeverity, string> = {
        blocking: strings.severityBlockingHelp,
        degraded: strings.severityDegradedHelp,
        informational: strings.severityInformationalHelp,
    }
    const codeLabels: Record<string, string> = {
        'reference-missing': strings.codeReferenceMissing,
        'reference-invalid': strings.codeReferenceInvalid,
        'alias-object-absent': strings.codeAliasObjectAbsent,
        'alias-object-mismatch': strings.codeAliasObjectMismatch,
        'record-invalid': strings.codeRecordInvalid,
        'record-orphan': strings.codeRecordOrphan,
        'object-unreferenced': strings.codeObjectUnreferenced,
        unclassified: strings.codeUnclassified,
        'intent-quarantined': strings.codeIntentQuarantined,
    }
    const codeHelp: Record<string, string> = {
        'intent-quarantined': strings.codeIntentQuarantinedHelp,
    }
    const severityColors: Record<DataHealthSeverity, string> = {
        blocking: 'bg-danger-400',
        degraded: 'bg-primary-300',
        informational: 'bg-borderc',
    }

    const count = (value: number): string => value.toLocaleString()
    const codeLabel = (code: string): string => codeLabels[code] ?? code
    const itemLocation = (item: DataHealthFinding): string =>
        [item.owner.id || item.owner.kind, item.locator?.sourcePath]
            .filter(Boolean)
            .join(' · ')
    const itemTarget = (item: DataHealthFinding): string =>
        item.target ? `${item.target.kind}: ${item.target.key}` : ''
    const separatedFieldLabel = (field: string): string => ({
        characters: strings.rootFieldCharacters,
        botPresets: strings.rootFieldPresets,
        pluginCustomStorage: strings.rootFieldPluginStorage,
        pluginStorageMeta: strings.rootFieldPluginStorageMeta,
    })[field] ?? field
    const presentationStrings = {
        rootSeparatedField: (field: string) =>
            strings.rootSeparatedField.replace('{0}', separatedFieldLabel(field)),
        moduleAssetMissing: (module: string, ordinal: number) =>
            strings.moduleAssetMissing
                .replace('{0}', module)
                .replace('{1}', count(ordinal)),
        conversationModuleMissing: (character: string, conversation: string) =>
            strings.conversationModuleMissing
                .replace('{0}', character)
                .replace('{1}', conversation),
        conversationMessageInlayMissing: (
            character: string,
            conversation: string,
            ordinal: number,
        ) => strings.conversationMessageInlayMissing
            .replace('{0}', character)
            .replace('{1}', conversation)
            .replace('{2}', count(ordinal)),
    }
    const itemDescription = (item: DataHealthFinding): string | null =>
        describeDataHealthFinding(
            item,
            resolvedNames.get(dataHealthOwnerKey(item.owner.kind, item.owner.id)) ?? null,
            presentationStrings,
        )

    async function refreshNames(items: readonly DataHealthFinding[]): Promise<void> {
        const generation = ++namesGeneration
        try {
            const resolver = resolveNames ?? (
                await import('src/ts/storage/dataHealthNames')
            ).resolveDataHealthFindingNames
            const names = await resolver(items)
            if (generation === namesGeneration) resolvedNames = names
        } catch {
            if (generation === namesGeneration) resolvedNames = new Map()
        }
    }

    let deepProgress = $derived.by(() => {
        const deep = view.result?.deep
        if (!deep) return ''
        const percent = `${Math.round((view.deepFraction ?? 0) * 100)}%`
        const objects = strings.deepProgress
            .replace('{0}', count(deep.completedObjects))
            .replace('{1}', count(deep.totalObjects))
            .replace('{2}', percent)
        const bytes = strings.deepBytes
            .replace('{0}', formatRisuNestStorageBytes(deep.completedBytes))
            .replace('{1}', formatRisuNestStorageBytes(deep.totalBytes))
        return `${objects} · ${bytes}`
    })
    // While a deep scan runs its pages report the share done; a quick scan reports nothing until it ends.
    let scanDetail = $derived.by(() => {
        const deep = view.result?.deep
        if (view.running !== 'deep' || !deep) return ''
        return strings.deepBytes
            .replace('{0}', formatRisuNestStorageBytes(deep.completedBytes))
            .replace('{1}', formatRisuNestStorageBytes(deep.totalBytes))
    })
    let summaryLines = $derived.by(() => {
        const result = view.result
        if (!result) return [strings.never]
        const lines = [
            strings.lastScan.replace(
                '{0}',
                new Date(result.scannedAt).toLocaleString(),
            ),
            strings.dataVersion.replace('{0}', count(result.revision)),
            result.depth === 'quick'
                ? strings.quickOnly
                : result.deep?.complete
                  ? strings.deepDone
                  : strings.deepStopped,
        ]
        return lines
    })

    function reportText(): string {
        const result = view.result
        return result
            ? formatDataHealthReport(result, {
                  includeNames,
                  resolveOwnerName: (kind, id) =>
                      resolvedNames.get(dataHealthOwnerKey(kind, id))?.ownerName ?? null,
              })
            : ''
    }

    async function copyReport(): Promise<void> {
        try {
            await navigator.clipboard.writeText(reportText())
            alertNormal(strings.copied)
        } catch {
            alertError(strings.copyFailed)
        }
    }

    async function saveReport(): Promise<void> {
        const result = view.result
        if (!result) return
        try {
            if (await downloadFile(dataHealthReportFileName(result), reportText())) {
                alertNormal(strings.saved)
            }
        } catch {
            alertError(strings.saveFailed)
        }
    }

    async function run(action: () => Promise<void>): Promise<void> {
        try {
            await action()
        } catch {
            // The model owns the operation-specific failure message.
        }
    }

    const actionLabels: Record<RepairCandidate['action']['action'], string> = {
        'drop-reference': strings.actionDropReference,
        'drop-alias': strings.actionDropAlias,
        'adopt-stored-payload': strings.actionAdoptStoredPayload,
        'normalize-records': strings.actionNormalizeRecords,
        'select-first-conversation': strings.actionSelectFirstConversation,
        'select-first-preset': strings.actionSelectFirstPreset,
        'select-first-persona': strings.actionSelectFirstPersona,
        'assign-embedded-module-id': strings.actionAssignEmbeddedModuleId,
        'keep-single-record': strings.actionKeepSingleRecord,
        'recover-orphans': strings.actionRecoverOrphans,
    }
    let choices = $derived(repairChoicesByFinding(view.candidates))
    // Select all is checked once every finding has an answer chosen.
    let answeredFindings = $derived(
        new Set(
            view.candidates
                .filter((candidate) => view.selection.includes(candidate.id))
                .map((candidate) => candidate.finding),
        ).size,
    )
    let allSelected = $derived(choices.size > 0 && answeredFindings === choices.size)
    let someSelected = $derived(answeredFindings > 0 && !allSelected)
    // The snapshot suggestion follows the selection, until the reader decides for themselves.
    let snapshotTouched = $state(false)
    $effect(() => {
        if (!snapshotTouched) keepSnapshot = view.preview?.proposesSnapshot ?? false
    })
    let showRepair = $derived(
        Boolean(view.result) && !view.outdated && (view.groups.length > 0 || view.journals.length > 0),
    )

    function findingLabel(index: number): string {
        const item = view.result?.items[index]
        return item ? `${codeLabel(item.code)} · ${itemLocation(item)}` : ''
    }

    async function applyRepair(): Promise<void> {
        await run(() => model.apply(keepSnapshot))
    }

    async function discardIntent(item: DataHealthFinding): Promise<void> {
        const diagnosis = view.result
        if (!diagnosis || view.activity) return
        const confirmed = await alertCheckboxConfirm({
            title: strings.discardIntentTitle,
            description: strings.discardIntentDescription,
            checkboxLabel: strings.discardIntentAcknowledge,
            actionLabel: strings.discardIntent,
            cancelLabel: language.cancel,
            requireChecked: true,
        })
        if (confirmed.confirmed && confirmed.checked) await run(() => model.discard(item, diagnosis))
    }

    async function completeIntent(item: DataHealthFinding): Promise<void> {
        const diagnosis = view.result
        if (!diagnosis || view.activity) return
        const confirmed = await alertCheckboxConfirm({
            title: strings.completeIntentTitle,
            description: strings.completeIntentDescription,
            checkboxLabel: strings.completeIntentAcknowledge,
            actionLabel: strings.completeIntent,
            cancelLabel: language.cancel,
            requireChecked: true,
        })
        if (confirmed.confirmed && confirmed.checked) await run(() => model.complete(item, diagnosis))
    }

    async function undoRepair(id: string): Promise<void> {
        await run(() => model.undo(id))
    }

    let resolvedResult: typeof view.result = null
    const unsubscribe = model.subscribe((next) => {
        view = next
        if (next.result !== resolvedResult) {
            resolvedResult = next.result
            void refreshNames(next.result?.items ?? [])
        }
    })
    let preparation = $state<'pending' | 'ready' | 'failed'>('pending')
    async function initialize(): Promise<void> {
        preparation = 'pending'
        try {
            await prepare?.()
            preparation = 'ready'
            await model.load()
            await model.loadRepairs()
        } catch { preparation = 'failed' }
    }
    const failureMessages = {
        discard: strings.discardIntentFailed,
        complete: strings.completeIntentFailed,
        load: strings.loadFailed, scan: strings.scanFailed, repair: strings.repairFailed,
        undo: strings.undoFailed, refresh: strings.refreshFailed, preview: strings.previewFailed,
    }
    onMount(() => { void initialize() })
    onDestroy(() => {
        namesGeneration++
        unsubscribe()
    })
</script>

<SettingGroup
    id="risunest-data-health"
    title={strings.title}
    description={strings.description}
    panelProps={{ 'data-data-health': '' }}
>
    <div data-data-health-summary class="px-4 py-3">
        <div role="status" aria-live="polite">
            <p class="text-sm text-textcolor2">{summaryLines.join(' · ')}</p>
            {#if view.result}
                {#if view.result.items.length === 0}
                    <p class="mt-1 text-[15px]">{strings.healthy}</p>
                {:else}
                    <ul class="mt-2 flex flex-wrap gap-x-5 gap-y-1 text-sm">
                        {#each ['blocking', 'degraded', 'informational'] as const as severity (severity)}
                            <li class="flex items-center gap-2">
                                <span class="h-2.5 w-2.5 shrink-0 rounded-xs {severityColors[severity]}" aria-hidden="true"></span>
                                <span class="text-textcolor2">{severityLabels[severity]}</span>
                                <span class="tabular-nums">{count(view.result.counts[severity])}</span>
                            </li>
                        {/each}
                    </ul>
                {/if}
                {#if view.result.omitted > 0}
                    <p class="mt-1 text-sm text-textcolor2">{strings.omitted.replace('{0}', count(view.result.omitted))}</p>
                {/if}
                {#if deepProgress && !view.running}
                    <p class="mt-1 text-sm text-textcolor2 tabular-nums">{deepProgress}</p>
                {/if}
            {/if}
            {#if preparation === 'failed'}
                <p class="mt-1 text-sm text-danger-400" role="alert">{strings.openFailed}</p><SettingButton class="mt-2" onclick={initialize}>{language.retry}</SettingButton>
            {/if}
            {#if view.applied}<p class="mt-1 text-sm" role="status">{strings.repairApplied.replace('{0}', count(view.applied.remaining))}</p>{/if}
            {#if view.outdated}<p class="mt-1 text-sm">{strings.outdated}</p>{/if}
            {#if view.failure}
                <p class="mt-1 text-sm text-danger-400" role="alert">{failureMessages[view.failure]}</p>
            {/if}
        </div>
        {#if view.running}
            <div data-data-health-progress class="mt-3">
                <SettingProgress
                    label={view.running === 'deep' ? strings.deepScanRunning : strings.quickScanRunning}
                    detail={scanDetail}
                    fraction={view.running === 'deep' ? view.deepFraction : null}
                >
                    {#snippet actions()}
                        {#if view.running === 'deep'}
                            <SettingButton variant="secondary" onclick={() => run(() => model.cancel())}>{strings.cancel}</SettingButton>
                        {/if}
                    {/snippet}
                </SettingProgress>
            </div>
        {/if}
    </div>

    <SettingRow data-data-health-actions label={strings.quickScan} help={strings.quickScanHelp}>
        <SettingButton
            variant="secondary"
            busy={view.running === 'quick'}
            disabled={preparation !== 'ready' || Boolean(view.activity) || view.loading}
            onclick={() => run(() => model.quickScan())}
        >{strings.quickScan}</SettingButton>
    </SettingRow>
    <SettingRow data-data-health-deep label={strings.deepScan} help={strings.deepScanHelp}>
        <!-- Stopping a running check is offered once, beside the progress bar above. -->
        {#if view.resumable}
            <SettingButton variant="secondary" disabled={preparation !== 'ready' || Boolean(view.activity) || view.loading} onclick={() => run(() => model.deepScan(true))}>{strings.resume}</SettingButton>
        {/if}
        <SettingButton
            busy={view.running === 'deep'}
            disabled={preparation !== 'ready' || Boolean(view.activity) || view.loading}
            onclick={() => run(() => model.deepScan(false))}
        >{view.resumable ? strings.restart : strings.deepScan}</SettingButton>
    </SettingRow>

    {#each view.groups as group (group.severity + group.code)}
        <details data-data-health-group class="group">
            <summary class="flex cursor-pointer list-none items-center gap-2 px-4 py-2.5 text-[15px] select-none [&::-webkit-details-marker]:hidden">
                <ChevronRight size={16} class="shrink-0 text-textcolor2 transition-transform duration-200 group-open:rotate-90" aria-hidden="true" />
                <span class="h-2.5 w-2.5 shrink-0 rounded-xs {severityColors[group.severity]}" aria-hidden="true"></span>
                <span class="min-w-0 break-words">{codeLabel(group.code)}</span>
                <span class="ml-auto shrink-0 text-sm text-textcolor2 tabular-nums">{count(group.total)}</span>
            </summary>
            <p class="border-t border-darkborderc/55 px-4 py-2 pl-10 text-sm text-textcolor2">{codeHelp[group.code] ?? severityHelp[group.severity]}</p>
            {#each group.shown as item, index (index)}
                <div data-data-health-item class="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 border-t border-darkborderc/55 py-1.5 pr-4 pl-10 text-sm">
                    <span class="min-w-0 flex-1 break-all">{itemDescription(item) ?? itemLocation(item)}</span>
                    {#if item.code === 'intent-quarantined' && item.owner.kind === 'intent' && item.locator && item.intentAction === 'discard'}
                        <SettingButton variant="secondary" disabled={preparation !== 'ready' || Boolean(view.activity)} onclick={() => discardIntent(item)}>{strings.discardIntent}</SettingButton>
                    {:else if item.code === 'intent-quarantined' && item.owner.kind === 'intent' && item.locator && item.intentAction === 'complete'}
                        <SettingButton variant="secondary" disabled={preparation !== 'ready' || Boolean(view.activity)} onclick={() => completeIntent(item)}>{strings.completeIntent}</SettingButton>
                    {/if}
                    {#if !itemDescription(item) && itemTarget(item)}
                        <span class="min-w-0 break-all text-textcolor2">{itemTarget(item)}</span>
                    {/if}
                </div>
            {/each}
            {#if group.hidden > 0}
                <p class="border-t border-darkborderc/55 py-2 pr-4 pl-10 text-sm text-textcolor2">{strings.groupMore.replace('{0}', count(group.hidden))}</p>
            {/if}
            {#if group.code === 'object-unreferenced' && onOpenUnusedImages}
                <div class="border-t border-darkborderc/55 py-2 pr-4 pl-10">
                    <SettingButton variant="secondary" onclick={onOpenUnusedImages}>{strings.gcLink}</SettingButton>
                </div>
            {/if}
        </details>
    {/each}

    {#if view.groups.length > 0}
        <SettingRow data-data-health-report label={strings.copyReport} help={strings.includeNamesWarning}>
            {#snippet below()}
                <div class="mt-1.5">
                    <SettingToggle bind:checked={includeNames} label={strings.includeNames} showLabel />
                </div>
            {/snippet}
            <SettingButton variant="secondary" onclick={copyReport}>{strings.copyReport}</SettingButton>
            <SettingButton variant="secondary" onclick={saveReport}>{strings.saveReport}</SettingButton>
        </SettingRow>
    {/if}
</SettingGroup>

{#if showRepair}
    <SettingGroup id="risunest-data-health-repair" title={strings.repairTitle} description={strings.repairHelp} panelProps={{ 'data-data-health-repair': '' }}>
        {#if view.candidates.length === 0}
            <p class="px-4 py-3 text-sm text-textcolor2">{strings.repairNone}</p>
        {:else}
            <div data-data-health-select-all class="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 px-4 py-3">
                <SettingToggle
                    label={strings.selectAll}
                    showLabel
                    checked={allSelected}
                    indeterminate={someSelected}
                    disabled={preparation !== 'ready' || Boolean(view.activity)}
                    onchange={(next) => { void model.setAll(next) }}
                />
                <span class="text-sm text-textcolor2 tabular-nums">{strings.repairSelected.replace('{0}', count(view.selection.length))}</span>
            </div>
            {#each [...choices] as [finding, options] (finding)}
                <div data-data-health-choice class="px-4 py-3">
                    <p class="text-sm break-words">{findingLabel(finding)}</p>
                    <div class="mt-1.5 flex flex-col gap-1">
                        {#each options as option (option.id)}
                            <SettingToggle
                                checked={view.selection.includes(option.id)}
                                showLabel
                                disabled={preparation !== 'ready' || Boolean(view.activity)}
                                label={actionLabels[option.action.action]}
                                onchange={() => { void model.toggle(option.id) }}
                            />
                            {#if option.discards && view.selection.includes(option.id)}
                                <p class="pl-7 text-xs text-textcolor2">{strings.actionDiscards}</p>
                            {/if}
                            {#if option.action.action === 'adopt-stored-payload' && view.selection.includes(option.id)}
                                <p class="pl-7 text-xs text-textcolor2">{strings.actionRisky}</p>
                            {/if}
                        {/each}
                    </div>
                </div>
            {/each}
            <SettingRow
                data-data-health-apply
                label={strings.repairSelected.replace('{0}', count(view.selection.length))}
                help={strings.repairSnapshotHelp.replace('{0}', count(view.selection.length))}
            >
                {#snippet below()}
                    <div class="mt-1.5">
                        <SettingToggle
                            checked={keepSnapshot}
                            showLabel
                            disabled={preparation !== 'ready' || Boolean(view.activity)}
                            label={strings.repairSnapshot}
                            onchange={(next) => { snapshotTouched = true; keepSnapshot = next }}
                        />
                    </div>
                    {#if view.repairing}
                        <div data-data-health-repair-progress class="mt-3">
                            <SettingProgress label={strings.repairRunning} />
                        </div>
                    {/if}
                {/snippet}
                <SettingButton busy={view.repairing} disabled={preparation !== "ready" || Boolean(view.activity) || view.selection.length === 0} onclick={applyRepair}>{strings.repairApply}</SettingButton>
            </SettingRow>
        {/if}
        {#if view.skipped.length > 0}
            <p data-data-health-skipped class="px-4 py-3 text-sm text-textcolor2" role="status">{strings.undoSkipped.replace('{0}', view.skipped.join(', '))}</p>
        {/if}
        <div data-data-health-journals class="divide-y divide-darkborderc/55">
            <SettingRow label={strings.undoTitle} help={strings.undoHelp} />
            {#each view.journals as entry (entry.id)}
                <div class="flex flex-wrap items-center gap-3 px-4 py-2.5 text-sm">
                    <span class="min-w-0 flex-1 break-words tabular-nums">{strings.undoEntry
                        .replace('{0}', new Date(entry.createdAt).toLocaleString())
                        .replace('{1}', count(entry.changes))
                        .replace('{2}', count(entry.heldObjects))}</span>
                    <SettingButton variant="secondary" disabled={preparation !== 'ready' || Boolean(view.activity) || !entry.current} onclick={() => undoRepair(entry.id)}>{strings.undoAction}</SettingButton>
                </div>
            {:else}
                <p class="px-4 py-2.5 text-sm text-textcolor2">{strings.undoNone}</p>
            {/each}
        </div>
    </SettingGroup>
{/if}
