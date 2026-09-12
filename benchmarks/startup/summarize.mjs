import { readFile } from 'node:fs/promises'
import { maxConcurrentResources, percentile } from './metrics.mjs'

const result = JSON.parse(await readFile(process.argv[2], 'utf8'))
const groups = new Map()
for (const sample of result.samples.filter((s) => !s.warmup)) {
    const key = `${sample.assets}/${sample.mode}/${sample.kind}`
    if (!groups.has(key)) groups.set(key, [])
    groups.get(key).push(sample)
}
const distribution = (values) => ({
    measured: values.filter(Number.isFinite).length,
    p50: percentile(values, 50),
    p95: percentile(values, 95),
    max: percentile(values, 100),
})
const sum = (values) => values.reduce((a, b) => a + b, 0)
const openTotal = (sample, field) => {
    const calls = sample.calls.filter((c) => c.command === 'pds_open')
    return calls.length && calls.every((c) => Number.isFinite(c[field]))
        ? sum(calls.map((c) => c[field]))
        : null
}
const preparationCanonical = (sample) => {
    const materialize = sample.calls.find((c) => c.command === 'pds_materialize')
    const baseline = sample.operations.find((o) => o.stage === 'baseline')
    if (!materialize || !Number.isFinite(materialize.ms) || !baseline) return null
    return sample.operations.filter(
        (o) =>
            o.stage === 'canonical' &&
            o.start >= materialize.start + materialize.ms &&
            o.start < baseline.start,
    )
}
const output = []
for (const samples of groups.values()) {
    const first = samples[0]
    const operations = {}
    for (const stage of [
        'prepare-import',
        'prepare-bootstrap',
        'normalize',
        'canonical',
        'baseline',
        'capture',
        'capture-database',
        'flush',
        'clean-chunks',
    ]) {
        operations[stage] = distribution(
            samples.map((s) => sum(s.operations.filter((o) => o.stage === stage).map((o) => o.ms))),
        )
    }
    const snapshotOverlap = samples.flatMap((s) => {
        const snapshots = s.calls.filter(
            (c) => c.command === 'pds_snapshot_create' && c.ms !== null,
        )
        return s.calls
            .filter(
                (c) =>
                    c.command.startsWith('pds_') &&
                    !c.command.startsWith('pds_snapshot_') &&
                    snapshots.some(
                        (snapshot) =>
                            c.start >= snapshot.start && c.start < snapshot.start + snapshot.ms,
                    ),
            )
            .map((c) => c.ms)
    })
    output.push({
        assets: first.assets,
        mode: first.mode,
        kind: first.kind,
        count: samples.length,
        interactiveMs: distribution(samples.map((s) => s.interactiveMs)),
        bootstrapToInteractiveMs: distribution(
            samples.map(
                (s) => s.interactiveMs - s.operations.find((o) => o.stage === 'bootstrap')?.start,
            ),
        ),
        launchToReadyMs: distribution(samples.map((s) => s.launchToReadyMs)),
        launchToFirstPaintMs: distribution(samples.map((s) => s.launchToFirstPaintMs)),
        openMs: distribution(samples.map((s) => openTotal(s, 'ms'))),
        openBytes: distribution(samples.map((s) => openTotal(s, 'bytes'))),
        preparationCanonicalCount: distribution(
            samples.map((s) => preparationCanonical(s)?.length ?? null),
        ),
        preparationCanonicalMs: distribution(
            samples.map((s) => {
                const calls = preparationCanonical(s)
                return calls ? sum(calls.map((o) => o.ms)) : null
            }),
        ),
        firstSelectionMs: distribution(samples.map((s) => s.interaction?.firstSelectionMs)),
        firstScrollReadyMs: distribution(samples.map((s) => s.interaction?.firstScrollReadyMs)),
        firstInteractionCompleteMs: distribution(
            samples.map((s) =>
                Number.isFinite(s.interaction?.firstScrollReadyMs) &&
                Number.isFinite(s.interaction?.scrollMs)
                    ? s.interaction.firstScrollReadyMs + s.interaction.scrollMs
                    : null,
            ),
        ),
        selectionMs: distribution(samples.map((s) => s.interaction?.selectionMs)),
        inputMs: distribution(samples.map((s) => s.interaction?.inputMs)),
        scrollMs: distribution(samples.map((s) => s.interaction?.scrollMs)),
        interactionFailures: samples.filter((s) => s.interaction && !s.interaction.success).length,
        stabilizationTimeouts: samples.filter((s) => s.stabilizationTimeout).length,
        commits: distribution(
            samples.map((s) => s.calls.filter((c) => c.command === 'pds_commit').length),
        ),
        replacements: distribution(
            samples.map((s) => s.calls.filter((c) => c.command === 'pds_replace_commit').length),
        ),
        revisionDelta: distribution(
            samples.map((s) =>
                Number.isFinite(s.lastRevision) && Number.isFinite(s.firstRevision)
                    ? s.lastRevision - s.firstRevision
                    : null,
            ),
        ),
        gcCalls: sum(
            samples.map(
                (s) => s.calls.filter((c) => c.command === 'pds_asset_gc_maintenance').length,
            ),
        ),
        snapshotMs: distribution(
            samples.flatMap((s) =>
                s.calls.filter((c) => c.command === 'pds_snapshot_create').map((c) => c.ms),
            ),
        ),
        foregroundIpcDuringSnapshotMs: distribution(snapshotOverlap),
        sampledPeakHeapBytes: distribution(samples.map((s) => s.sampledPeakHeapBytes)),
        longestTaskMs: distribution(
            samples.map((s) => Math.max(0, ...s.longTasks.map((t) => t.ms))),
        ),
        mediaRequests: distribution(samples.map((s) => s.imageResources?.length)),
        maxImageRequests: distribution(
            samples.map((s) => maxConcurrentResources(s.imageResources ?? [])),
        ),
        operations,
    })
}
console.log(JSON.stringify(output, null, 2))
