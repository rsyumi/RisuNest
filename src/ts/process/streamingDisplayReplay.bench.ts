import { bench, expect, vi } from 'vitest'
import type { character, customscript } from '../storage/database.svelte'
import { createStreamingDisplayController } from './streamingDisplayScheduler'
import { fnv1a, makeRegexFixture } from './tests/phase1Fixtures'

const mocks = vi.hoisted(() => {
    const state = {
        currentSnapshotIndex: -1,
        effectOrder: [] as number[],
        emotions: {} as Record<string, [string, string, number][]>,
        luaActionCount: 0,
        pluginActionCount: 0,
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
        pluginAction: async (data: string) => {
            state.pluginActionCount += 1
            return data
        },
        database: {
            dynamicAssets: false,
            presetRegex: [] as customscript[],
            characters: [] as never[],
        },
    }
})

vi.mock('svelte/store', async (importOriginal) => {
    const original = await importOriginal<typeof import('svelte/store')>()
    return {
        ...original,
        get: (store: unknown) => store === mocks.charEmotionStore ? mocks.state.emotions : 0,
    }
})
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
    runLuaEditTrigger: async (_char: unknown, _mode: unknown, data: string) => {
        mocks.state.luaActionCount += 1
        return data
    },
}))
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    pluginV2: { editinput: new Set(), editoutput: new Set([mocks.pluginAction]), editprocess: new Set(), editdisplay: new Set() },
}))
vi.mock('src/ts/process/triggers', () => ({ runTrigger: vi.fn() }))

const { processScriptFull, resetScriptCache } = await import('./scripts')

type StreamingMode = 'off' | 'balanced' | 'strong'
type FixtureName = 'ordinary' | 'stateful-emotion'
type ReplayProfileName = 'steady' | 'burst' | 'sparse' | 'slow-pass'

interface ReplayProfile {
    name: ReplayProfileName
    processingMs: number
    arrivalAt(index: number): number
}

interface ReplayRun {
    finalHash: string
    sideEffectOrder: number[]
    luaActionCount: number
    pluginActionCount: number
    firstDisplayMs: number
    firstWorkStartMs: number
    totalProcessingMs: number
    displayUpdateCount: number
    activeMaximum: number
    pendingMaximum: number
    finalFlushCount: number
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
    profile: ReplayProfileName
    simulatedProcessingMs: number
    finalHash: string
    sideEffectCount: number
    luaActionCount: number
    pluginActionCount: number
    sideEffectOrderHash: string
    sideEffectOrder: string
    displayUpdateCount: number
    firstDisplayMs: TimingSummary
    firstWorkStartMs: TimingSummary
    activeMaximum: number
    pendingMaximum: number
    finalFlushCount: number
    totalProcessingMs: TimingSummary
    longTaskCount: number
    longTaskTotalMs: TimingSummary
    maxTaskMs: TimingSummary
}

const STREAMING_DISPLAY_FLUSH_DELAY_MS = 125
const CHUNK_INTERVAL_MS = 10
const LONG_TASK_THRESHOLD_MS = 50
const MEASUREMENT_RUNS = 11
const replayProfiles: Record<ReplayProfileName, ReplayProfile> = {
    steady: {
        name: 'steady',
        processingMs: 0,
        arrivalAt: (index) => index * CHUNK_INTERVAL_MS,
    },
    burst: {
        name: 'burst',
        processingMs: 0,
        arrivalAt: (index) => Math.floor(index / 20) * 25,
    },
    sparse: {
        name: 'sparse',
        processingMs: 0,
        arrivalAt: (index) => index * 200,
    },
    'slow-pass': {
        name: 'slow-pass',
        processingMs: 175,
        arrivalAt: (index) => index * CHUNK_INTERVAL_MS,
    },
}

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

class ReplayClock {
    nowMs = 0
    nextTimerId = 1
    timers = new Map<number, { at: number; callback: () => void }>()

    readonly clock = {
        now: () => this.nowMs,
        setTimeout: (callback: () => void, delay: number) => {
            const id = this.nextTimerId++
            this.timers.set(id, { at: this.nowMs + delay, callback })
            return id as unknown as ReturnType<typeof setTimeout>
        },
        clearTimeout: (timer: ReturnType<typeof setTimeout>) => {
            this.timers.delete(timer as unknown as number)
        },
    }

    async advanceTo(targetMs: number) {
        targetMs = Math.max(targetMs, this.nowMs)
        while(true){
            const due = [...this.timers.entries()]
                .filter(([, timer]) => timer.at <= targetMs)
                .sort((left, right) => left[1].at - right[1].at)[0]
            if(!due) break
            this.nowMs = due[1].at
            this.timers.delete(due[0])
            due[1].callback()
            await settleScheduler()
        }
        this.nowMs = targetMs
    }

    elapse(durationMs: number) {
        this.nowMs += durationMs
    }

    wait(durationMs: number): Promise<void> {
        return new Promise((resolve) => {
            this.clock.setTimeout(resolve, durationMs)
        })
    }

    async runAll() {
        while(true){
            await settleScheduler()
            const next = [...this.timers.values()]
                .sort((left, right) => left.at - right.at)[0]
            if(!next) return
            await this.advanceTo(next.at)
        }
    }
}

async function settleScheduler() {
    for(let index = 0; index < 8; index++) await Promise.resolve()
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

async function runReplay(
    chunkCount: number,
    fixture: FixtureName,
    mode: StreamingMode,
    profile: ReplayProfile,
): Promise<ReplayRun> {
    const snapshots = makeSnapshots(chunkCount)
    const character = makeCharacter(fixture)
    const taskDurations: number[] = []
    const clock = new ReplayClock()
    let firstWorkStartMs = Number.POSITIVE_INFINITY
    let firstDisplayMs = Number.POSITIVE_INFINITY
    let displayUpdateCount = 0
    let scheduledStartCount = 0
    let activeCount = 0
    let activeMaximum = 0
    let pendingMaximum = 0
    let finalData = ''

    resetScriptCache()
    mocks.state.currentSnapshotIndex = -1
    mocks.state.effectOrder = []
    mocks.state.emotions = {}
    mocks.state.luaActionCount = 0
    mocks.state.pluginActionCount = 0

    const processSemantic = async ({ sequence, value }: { sequence: number; value: string }) => {
        activeCount += 1
        activeMaximum = Math.max(activeMaximum, activeCount)
        const snapshotIndex = sequence - 1
        scheduledStartCount += mode === 'strong' ? 0 : 1
        firstWorkStartMs = Math.min(firstWorkStartMs, clock.nowMs)
        mocks.state.currentSnapshotIndex = snapshotIndex
        const startedAt = performance.now()
        try {
            const result = await processScriptFull(
                character,
                value,
                'editoutput',
                -1,
                {},
                { cache: 'bypass', regexWorker: false },
            )
            taskDurations.push(performance.now() - startedAt)
            if(profile.processingMs > 0){
                if(mode === 'balanced') await clock.wait(profile.processingMs)
                else clock.elapse(profile.processingMs)
            }
            finalData = result.data
            displayUpdateCount += 1
            firstDisplayMs = Math.min(firstDisplayMs, clock.nowMs)
        }
        finally {
            activeCount -= 1
        }
    }
    const controller = createStreamingDisplayController({
        mode,
        clock: clock.clock,
        intervalMs: STREAMING_DISPLAY_FLUSH_DELAY_MS,
        processSemantic,
        processPreview: async ({ sequence, value }) => {
            activeCount += 1
            activeMaximum = Math.max(activeMaximum, activeCount)
            scheduledStartCount += 1
            firstWorkStartMs = Math.min(firstWorkStartMs, clock.nowMs)
            mocks.state.currentSnapshotIndex = sequence - 1
            finalData = value
            displayUpdateCount += 1
            firstDisplayMs = Math.min(firstDisplayMs, clock.nowMs)
            activeCount -= 1
        },
    })

    for(let index = 0; index < snapshots.length; index++){
        await clock.advanceTo(profile.arrivalAt(index))
        await controller.submit(snapshots[index])
        await settleScheduler()
        const state = controller.inspect()
        activeMaximum = Math.max(activeMaximum, state.activeCount)
        pendingMaximum = Math.max(pendingMaximum, state.pendingCount)
    }
    const startsBeforeFinish = scheduledStartCount
    const finishing = controller.finish()
    await clock.runAll()
    await finishing
    await settleScheduler()
    const finalFlushCount = scheduledStartCount - startsBeforeFinish

    const longTasks = taskDurations.filter((duration) => duration >= LONG_TASK_THRESHOLD_MS)

    return {
        finalHash: fnv1a(finalData),
        sideEffectOrder: [...mocks.state.effectOrder],
        luaActionCount: mocks.state.luaActionCount,
        pluginActionCount: mocks.state.pluginActionCount,
        firstDisplayMs,
        firstWorkStartMs,
        totalProcessingMs: taskDurations.reduce((total, duration) => total + duration, 0),
        displayUpdateCount,
        activeMaximum,
        pendingMaximum,
        finalFlushCount,
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

async function measureReplay(
    chunkCount: number,
    fixture: FixtureName,
    mode: StreamingMode,
    profile: ReplayProfile,
): Promise<ReplayMeasurement> {
    const runs: ReplayRun[] = []
    for(let run = 0; run < MEASUREMENT_RUNS; run++){
        runs.push(await runReplay(chunkCount, fixture, mode, profile))
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
        profile: profile.name,
        simulatedProcessingMs: profile.processingMs,
        finalHash: reference.finalHash,
        sideEffectCount: reference.sideEffectOrder.length,
        luaActionCount: reference.luaActionCount,
        pluginActionCount: reference.pluginActionCount,
        sideEffectOrderHash: fnv1a(reference.sideEffectOrder.join(',')),
        sideEffectOrder: formatSideEffectOrder(reference.sideEffectOrder, chunkCount, mode),
        displayUpdateCount: reference.displayUpdateCount,
        firstDisplayMs: summarize(measuredRuns.map((run) => run.firstDisplayMs)),
        firstWorkStartMs: summarize(measuredRuns.map((run) => run.firstWorkStartMs)),
        activeMaximum: Math.max(...measuredRuns.map((run) => run.activeMaximum)),
        pendingMaximum: Math.max(...measuredRuns.map((run) => run.pendingMaximum)),
        finalFlushCount: Math.max(...measuredRuns.map((run) => run.finalFlushCount)),
        totalProcessingMs: summarize(measuredRuns.map((run) => run.totalProcessingMs)),
        longTaskCount: Math.max(...measuredRuns.map((run) => run.longTaskCount)),
        longTaskTotalMs: summarize(measuredRuns.map((run) => run.longTaskTotalMs)),
        maxTaskMs: summarize(measuredRuns.map((run) => run.maxTaskMs)),
    }
}

const measurements: ReplayMeasurement[] = []
const measurementCases = [
    ...[20, 100, 500].map((chunkCount) => ({ chunkCount, profile: replayProfiles.steady })),
    ...(['burst', 'sparse', 'slow-pass'] as const).map((profile) => ({
        chunkCount: 100,
        profile: replayProfiles[profile],
    })),
]
for(const { chunkCount, profile } of measurementCases){
    for(const fixture of ['ordinary', 'stateful-emotion'] as const){
        for(const mode of ['off', 'balanced', 'strong'] as const){
            measurements.push(await measureReplay(chunkCount, fixture, mode, profile))
        }
    }
}

for(const { chunkCount, profile } of measurementCases){
    const caseMeasurements = measurements.filter((measurement) =>
        measurement.chunkCount === chunkCount && measurement.profile === profile.name)
    const ordinary = caseMeasurements.filter((measurement) => measurement.fixture === 'ordinary')
    expect(new Set(ordinary.map((measurement) => measurement.finalHash))).toEqual(new Set([regexFixture.expectedHash]))

    const stateful = caseMeasurements.filter((measurement) => measurement.fixture === 'stateful-emotion')
    expect(new Set(stateful.map((measurement) => measurement.finalHash))).toEqual(new Set([regexFixture.expectedHash]))
    const exactCount = stateful.find((measurement) => measurement.mode === 'off')?.sideEffectCount
    const balanced = stateful.find((measurement) => measurement.mode === 'balanced')!
    const balancedCount = balanced.sideEffectCount
    if(profile.name === 'sparse') expect(balancedCount).toBe(exactCount)
    else expect(balancedCount).not.toBe(exactCount)
    if(profile.name === 'burst') expect(balancedCount).toBe(2)
    if(profile.name === 'slow-pass'){
        expect(balanced.firstWorkStartMs.median).toBe(0)
        expect(balanced.firstDisplayMs.median).toBe(profile.processingMs)
    }
    for(const measurement of stateful){
        expect(measurement.luaActionCount).toBe(measurement.sideEffectCount)
        expect(measurement.pluginActionCount).toBe(measurement.sideEffectCount)
        expect(measurement.activeMaximum).toBeLessThanOrEqual(1)
        expect(measurement.pendingMaximum).toBeLessThanOrEqual(1)
    }
}

console.log(`STREAMING_DISPLAY_MEASUREMENTS ${JSON.stringify({
    method: {
        responseBytes: regexFixture.input.length,
        regexRuleCount: regexFixture.scripts.length,
        flushDelayMs: STREAMING_DISPLAY_FLUSH_DELAY_MS,
        scheduler: 'production-leading-edge',
        profiles: Object.values(replayProfiles).map((profile) => ({
            name: profile.name,
            processingMs: profile.processingMs,
        })),
        warmRuns: MEASUREMENT_RUNS,
        discardedRuns: 1,
        longTaskThresholdMs: LONG_TASK_THRESHOLD_MS,
    },
    measurements,
}, null, 2)}`)

bench('streaming display replay measurement gate is deterministic', () => {
    expect(measurements).toHaveLength(36)
})
