import { bench, expect, vi } from 'vitest'
import type { character, customscript } from '../storage/database.svelte'
import { fnv1a, makeRegexFixture } from './tests/phase1Fixtures'

const mocks = vi.hoisted(() => {
    const state = {
        currentSnapshotIndex: -1,
        effectOrder: [] as number[],
        emotions: {} as Record<string, [string, string, number][]>,
    }
    const charEmotionStore = {
        set(value: Record<string, [string, string, number][]>) {
            state.emotions = value
            state.effectOrder.push(state.currentSnapshotIndex)
        },
    }
    return {
        state,
        charEmotionStore,
        selectedCharStore: {},
        database: {
            dynamicAssets: false,
            presetRegex: [] as customscript[],
            characters: [] as never[],
        },
    }
})

vi.mock('svelte/store', () => ({
    get: (store: unknown) => store === mocks.charEmotionStore ? mocks.state.emotions : 0,
}))
vi.mock('src/ts/stores.svelte', () => ({
    CharEmotion: mocks.charEmotionStore,
    selectedCharID: mocks.selectedCharStore,
}))
vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => mocks.database,
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(),
}))
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('src/ts/util', () => ({ selectSingleFile: vi.fn() }))
vi.mock('src/ts/parser/parser.svelte', () => ({
    assetRegex: /$^/g,
    risuChatParser: (data: string) => data,
}))
vi.mock('src/ts/process/modules', () => ({
    getModuleAssets: () => [],
    getModuleRegexScripts: () => [],
}))
vi.mock('src/ts/process/memory/hypamemory', () => ({ HypaProcesser: class {} }))
vi.mock('src/ts/process/scriptings', () => ({
    runLuaEditTrigger: async (_char: unknown, _mode: unknown, data: string) => data,
}))
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    pluginV2: { editinput: new Set(), editoutput: new Set(), editprocess: new Set(), editdisplay: new Set() },
}))
vi.mock('src/ts/process/triggers', () => ({ runTrigger: vi.fn() }))

const { processScriptFull, resetScriptCache } = await import('./scripts')

type StreamingMode = 'off' | 'balanced' | 'strong'
type FixtureName = 'ordinary' | 'stateful-emotion'

interface ScheduledSnapshot {
    index: number
    displayAtMs: number
}

interface ReplayRun {
    finalHash: string
    sideEffectOrder: number[]
    firstDisplayMs: number
    totalProcessingMs: number
    displayUpdateCount: number
    longTaskCount: number
    longTaskTotalMs: number
    maxTaskMs: number
}

interface TimingSummary {
    median: number
    p95: number
}

interface ReplayMeasurement {
    chunkCount: number
    fixture: FixtureName
    mode: StreamingMode
    finalHash: string
    sideEffectCount: number
    sideEffectOrderHash: string
    sideEffectOrder: string
    displayUpdateCount: number
    firstDisplayMs: TimingSummary
    totalProcessingMs: TimingSummary
    longTaskCount: number
    longTaskTotalMs: TimingSummary
    maxTaskMs: TimingSummary
}

const STREAMING_DISPLAY_FLUSH_DELAY_MS = 125
const FRAME_INTERVAL_MS = 16
const CHUNK_INTERVAL_MS = 10
const LONG_TASK_THRESHOLD_MS = 50
const MEASUREMENT_RUNS = 11

const regexFixture = makeRegexFixture(100)
const snapshotsByCount = new Map<number, string[]>()

function makeSnapshots(chunkCount: number): string[] {
    const cached = snapshotsByCount.get(chunkCount)
    if(cached){
        return cached
    }
    const snapshots = Array.from({ length: chunkCount }, (_, index) => {
        const end = Math.floor(regexFixture.input.length * (index + 1) / chunkCount)
        return regexFixture.input.slice(0, end)
    })
    snapshotsByCount.set(chunkCount, snapshots)
    return snapshots
}

function nextFrameAt(timeMs: number): number {
    return Math.ceil(timeMs / FRAME_INTERVAL_MS) * FRAME_INTERVAL_MS
}

function selectDisplaySnapshots(chunkCount: number, mode: StreamingMode): ScheduledSnapshot[] {
    if(mode === 'off'){
        return Array.from({ length: chunkCount }, (_, index) => ({
            index,
            displayAtMs: index * CHUNK_INTERVAL_MS,
        }))
    }

    const selected: ScheduledSnapshot[] = []
    let pendingIndex: number | null = null
    let scheduledAtMs: number | null = null

    for(let index = 0; index < chunkCount; index++){
        const arrivedAtMs = index * CHUNK_INTERVAL_MS
        if(scheduledAtMs !== null && scheduledAtMs < arrivedAtMs){
            if(pendingIndex !== null){
                selected.push({ index: pendingIndex, displayAtMs: scheduledAtMs })
                pendingIndex = null
            }
            scheduledAtMs = null
        }
        pendingIndex = index
        scheduledAtMs ??= nextFrameAt(arrivedAtMs + STREAMING_DISPLAY_FLUSH_DELAY_MS)
    }

    if(pendingIndex !== null){
        selected.push({
            index: pendingIndex,
            displayAtMs: (chunkCount - 1) * CHUNK_INTERVAL_MS,
        })
    }
    return selected
}

function makeCharacter(fixture: FixtureName): character {
    const sideEffectScript: customscript = {
        comment: 'record every edit-output execution through the existing emotion action',
        in: '^',
        out: '@@emo happy',
        type: 'editoutput',
        flag: '',
        ableFlag: true,
    }
    return {
        type: 'character',
        chaId: `streaming-${fixture}`,
        customscript: fixture === 'ordinary'
            ? regexFixture.scripts
            : [sideEffectScript, ...regexFixture.scripts],
        emotionImages: [['happy', 'happy.png']],
    } as character
}

async function runReplay(chunkCount: number, fixture: FixtureName, mode: StreamingMode): Promise<ReplayRun> {
    const snapshots = makeSnapshots(chunkCount)
    const scheduledSnapshots = selectDisplaySnapshots(chunkCount, mode)
    const processedSnapshots = mode === 'strong'
        ? [{ index: chunkCount - 1, displayAtMs: (chunkCount - 1) * CHUNK_INTERVAL_MS }]
        : scheduledSnapshots
    const character = makeCharacter(fixture)
    const taskDurations: number[] = []
    let finalData = ''

    resetScriptCache()
    mocks.state.currentSnapshotIndex = -1
    mocks.state.effectOrder = []
    mocks.state.emotions = {}

    for(const snapshot of processedSnapshots){
        mocks.state.currentSnapshotIndex = snapshot.index
        const startedAt = performance.now()
        const result = await processScriptFull(
            character,
            snapshots[snapshot.index],
            'editoutput',
            -1,
            {},
            { cache: 'bypass', regexWorker: false },
        )
        taskDurations.push(performance.now() - startedAt)
        finalData = result.data
    }

    const firstDisplayMs = mode === 'strong'
        ? scheduledSnapshots[0].displayAtMs
        : scheduledSnapshots[0].displayAtMs + taskDurations[0]
    const longTasks = taskDurations.filter((duration) => duration >= LONG_TASK_THRESHOLD_MS)

    return {
        finalHash: fnv1a(finalData),
        sideEffectOrder: [...mocks.state.effectOrder],
        firstDisplayMs,
        totalProcessingMs: taskDurations.reduce((total, duration) => total + duration, 0),
        displayUpdateCount: scheduledSnapshots.length + (mode === 'strong' ? 1 : 0),
        longTaskCount: longTasks.length,
        longTaskTotalMs: longTasks.reduce((total, duration) => total + duration, 0),
        maxTaskMs: Math.max(...taskDurations),
    }
}

function percentile(values: number[], percentileValue: number): number {
    const sorted = [...values].sort((left, right) => left - right)
    return sorted[Math.ceil(percentileValue * sorted.length) - 1]
}

function summarize(values: number[]): TimingSummary {
    return {
        median: Number(percentile(values, 0.5).toFixed(3)),
        p95: Number(percentile(values, 0.95).toFixed(3)),
    }
}

function formatSideEffectOrder(order: number[], chunkCount: number, mode: StreamingMode): string {
    if(order.length === 0){
        return 'none'
    }
    if(mode === 'off'){
        return `0..${chunkCount - 1}`
    }
    return `[${order.join(',')}]`
}

async function measureReplay(chunkCount: number, fixture: FixtureName, mode: StreamingMode): Promise<ReplayMeasurement> {
    const runs: ReplayRun[] = []
    for(let run = 0; run < MEASUREMENT_RUNS; run++){
        runs.push(await runReplay(chunkCount, fixture, mode))
    }
    const measuredRuns = runs.slice(1)
    const reference = measuredRuns[0]

    for(const run of measuredRuns){
        expect(run.finalHash).toBe(reference.finalHash)
        expect(run.sideEffectOrder).toEqual(reference.sideEffectOrder)
        expect(run.displayUpdateCount).toBe(reference.displayUpdateCount)
    }

    return {
        chunkCount,
        fixture,
        mode,
        finalHash: reference.finalHash,
        sideEffectCount: reference.sideEffectOrder.length,
        sideEffectOrderHash: fnv1a(reference.sideEffectOrder.join(',')),
        sideEffectOrder: formatSideEffectOrder(reference.sideEffectOrder, chunkCount, mode),
        displayUpdateCount: reference.displayUpdateCount,
        firstDisplayMs: summarize(measuredRuns.map((run) => run.firstDisplayMs)),
        totalProcessingMs: summarize(measuredRuns.map((run) => run.totalProcessingMs)),
        longTaskCount: Math.max(...measuredRuns.map((run) => run.longTaskCount)),
        longTaskTotalMs: summarize(measuredRuns.map((run) => run.longTaskTotalMs)),
        maxTaskMs: summarize(measuredRuns.map((run) => run.maxTaskMs)),
    }
}

const measurements: ReplayMeasurement[] = []
for(const chunkCount of [20, 100, 500]){
    for(const fixture of ['ordinary', 'stateful-emotion'] as const){
        for(const mode of ['off', 'balanced', 'strong'] as const){
            measurements.push(await measureReplay(chunkCount, fixture, mode))
        }
    }
}

for(const chunkCount of [20, 100, 500]){
    const ordinary = measurements.filter((measurement) => measurement.chunkCount === chunkCount && measurement.fixture === 'ordinary')
    expect(new Set(ordinary.map((measurement) => measurement.finalHash))).toEqual(new Set([regexFixture.expectedHash]))

    const stateful = measurements.filter((measurement) => measurement.chunkCount === chunkCount && measurement.fixture === 'stateful-emotion')
    expect(new Set(stateful.map((measurement) => measurement.finalHash))).toEqual(new Set([regexFixture.expectedHash]))
    expect(stateful.find((measurement) => measurement.mode === 'balanced')?.sideEffectCount)
        .not.toBe(stateful.find((measurement) => measurement.mode === 'off')?.sideEffectCount)
}

console.log(`STREAMING_DISPLAY_MEASUREMENTS ${JSON.stringify({
    method: {
        responseBytes: regexFixture.input.length,
        regexRuleCount: regexFixture.scripts.length,
        chunkIntervalMs: CHUNK_INTERVAL_MS,
        flushDelayMs: STREAMING_DISPLAY_FLUSH_DELAY_MS,
        frameIntervalMs: FRAME_INTERVAL_MS,
        warmRuns: MEASUREMENT_RUNS,
        discardedRuns: 1,
        longTaskThresholdMs: LONG_TASK_THRESHOLD_MS,
    },
    measurements,
}, null, 2)}`)

bench('streaming display replay measurement gate is deterministic', () => {
    expect(measurements).toHaveLength(18)
})
