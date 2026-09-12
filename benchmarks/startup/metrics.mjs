const operations = new Set([
    'prepare-import',
    'prepare-bootstrap',
    'normalize',
    'canonical',
    'baseline',
    'capture',
    'capture-database',
    'flush',
    'bootstrap',
    'clean-chunks',
    'save-observer',
    'save-effect-root',
    'save-effect-character',
])
const commands = new Set([
    'pds_open',
    'pds_materialize',
    'pds_commit',
    'pds_replace_commit',
    'pds_read_root',
    'pds_query_characters',
    'pds_snapshot_create',
    'pds_snapshot_list',
    'pds_read_character',
    'pds_query_conversations',
    'pds_read_conversation_window',
    'pds_read_conversation',
    'pds_asset_gc_maintenance',
    'plugin:fs|read_dir',
    'plugin:fs|stat',
    'asset_cas_read_object',
    'asset_cas_read_object_range',
    'asset_cas_stat_object',
])
const phases = new Set(['storage', 'data', 'compatibility', 'plugins', 'sync'])
const marks = new Set([
    'boot:local-data-ready',
    'boot:account-ready',
    'boot:cold-storage-ready',
    'boot:plugins-ready',
    'boot:interactive',
])
const number = (value) => (typeof value === 'number' && Number.isFinite(value) ? value : null)
const list = (value) => (Array.isArray(value) ? value : [])

export function sanitizeMetrics(raw) {
    return {
        interactiveMs: number(raw.interactiveMs),
        firstPaintMs: number(raw.firstPaintMs),
        usedHeapBytes: number(raw.usedHeapBytes),
        sampledPeakHeapBytes: number(raw.sampledPeakHeapBytes),
        maxMediaInFlight: number(raw.maxMediaInFlight),
        documentVisible: raw.documentVisible === true,
        cleanChunksReason: number(raw.cleanChunksReason),
        imageResources: list(raw.imageResources).map((v) => ({
            start: number(v.start),
            ms: number(v.ms),
        })),
        firstRevision: number(raw.firstRevision),
        lastRevision: number(raw.lastRevision),
        stabilizationTimeout: raw.stabilizationTimeout === true,
        elapsedSeen: raw.elapsedSeen === true,
        elapsedVisibleAfterStartup: raw.elapsedVisibleAfterStartup === true,
        calls: list(raw.calls)
            .filter((v) => commands.has(v.command))
            .map((v) => ({
                command: v.command,
                start: number(v.start),
                ms: number(v.ms),
                bytes: number(v.bytes),
                success: v.success === true,
            })),
        operations: list(raw.operations)
            .filter((v) => operations.has(v.stage))
            .map((v) => ({
                stage: v.stage,
                start: number(v.start),
                ms: number(v.ms),
                success: v.success === true,
            })),
        phases: list(raw.phases)
            .filter((v) => phases.has(v.stage))
            .map((v) => ({ stage: v.stage, ms: number(v.ms) })),
        marks: list(raw.marks)
            .filter((v) => marks.has(v.stage))
            .map((v) => ({ stage: v.stage, ms: number(v.ms) })),
        longTasks: list(raw.longTasks).map((v) => ({ start: number(v.start), ms: number(v.ms) })),
        unfinishedOperations: Object.entries(raw.active ?? {})
            .filter(([key, value]) => operations.has(key) && value > 0)
            .map(([stage, count]) => ({ stage, count: number(count) })),
        interaction: raw.interaction
            ? {
                  selectionMs: number(raw.interaction.selectionMs),
                  inputMs: number(raw.interaction.inputMs),
                  scrollMs: number(raw.interaction.scrollMs),
                  success: raw.interaction.success === true,
                  inputAccepted: raw.interaction.inputAccepted === true,
                  scrollChanged: raw.interaction.scrollChanged === true,
                  scrollImmediateChanged: raw.interaction.scrollImmediateChanged === true,
                  failure: number(raw.interaction.failure),
                  firstUiReadyMs: number(raw.interaction.firstUiReadyMs),
                  firstSelectionMs: number(raw.interaction.firstSelectionMs),
                  firstScrollReadyMs: number(raw.interaction.firstScrollReadyMs),
              }
            : null,
    }
}

export function percentile(values, percent) {
    const sorted = values
        .filter((value) => typeof value === 'number' && Number.isFinite(value))
        .sort((a, b) => a - b)
    return sorted.length ? sorted[Math.ceil((sorted.length * percent) / 100) - 1] : null
}

export function maxConcurrentResources(resources) {
    const events = resources
        .flatMap(({ start, ms }) =>
            Number.isFinite(start) && Number.isFinite(ms) && ms > 0
                ? [
                      { at: start, delta: 1 },
                      { at: start + ms, delta: -1 },
                  ]
                : [],
        )
        .sort((a, b) => a.at - b.at || a.delta - b.delta)
    let active = 0,
        peak = 0
    for (const event of events) {
        active += event.delta
        peak = Math.max(peak, active)
    }
    return peak
}
