import {
    executeNativeRegexBatch,
    tryExecuteNativeRegexBatch,
} from '../../src/ts/process/nativeRegexBatch'
import { getRegexExecutionPlan } from '../../src/ts/process/regexExecutionPlan'
import { classifyRegexSafePlan } from '../../src/ts/process/regexSafePlan'
import { RegexWorkerClient } from '../../src/ts/process/regexWorkerClient'
import { fnv1a, type RegexFixtureSize } from '../../src/ts/process/tests/phase1Fixtures'
import { makeBoundedRegexFixture } from './fixture'

const ruleCounts: RegexFixtureSize[] = [20, 100, 500]
const inputTargets = [32 * 1024, 256 * 1024, 1024 * 1024]
const measuredSamples = 10

interface TimingSummary {
    samplesMs: number[]
    p50Ms: number
    p95Ms: number
}

interface PilotCell {
    rules: number
    inputBytes: number
    expectedHash: string
    worker: TimingSummary
    native: TimingSummary
    gate: {
        medianImprovement: number
        p95Improvement: number
        passed: boolean
    }
}

function percentile(sorted: number[], percentileValue: number): number {
    const index = Math.min(
        sorted.length - 1,
        Math.max(0, Math.ceil(sorted.length * percentileValue) - 1),
    )
    return sorted[index]
}

function summarize(samples: number[]): TimingSummary {
    const sorted = [...samples].sort((left, right) => left - right)
    return {
        samplesMs: samples,
        p50Ms: (sorted[4] + sorted[5]) / 2,
        p95Ms: percentile(sorted, 0.95),
    }
}

async function measure<T>(run: () => Promise<T>): Promise<{ result: T; elapsedMs: number }> {
    const started = performance.now()
    const result = await run()
    return { result, elapsedMs: performance.now() - started }
}

function assertResult(
    engine: string,
    result: { data: string; errors: unknown[] },
    expectedHash: string,
): void {
    if (result.errors.length > 0) {
        throw new Error(`${engine} returned ${result.errors.length} rule errors`)
    }
    const hash = fnv1a(result.data)
    if (hash !== expectedHash) {
        throw new Error(`${engine} output hash ${hash} did not match ${expectedHash}`)
    }
}

async function runCell(
    worker: RegexWorkerClient,
    rules: RegexFixtureSize,
    inputTarget: number,
): Promise<PilotCell> {
    const fixture = makeBoundedRegexFixture(rules, inputTarget)
    const plan = getRegexExecutionPlan(fixture.scripts, 'editoutput')
    const classification = classifyRegexSafePlan(plan, fixture.input)
    if (classification.accepted === false) {
        throw new Error(`Fixture was not Rust-safe: ${classification.category}`)
    }

    const runWorker = () => worker.execute(plan, fixture.input)
    const runNative = async () => {
        if (rules === 500 && inputTarget >= 256 * 1024) {
            const result = await tryExecuteNativeRegexBatch(plan, fixture.input)
            if (result === undefined) {
                throw new Error('Production route rejected an adopted benchmark cell')
            }
            return result
        }
        const current = classifyRegexSafePlan(plan, fixture.input)
        if (current.accepted === false) {
            throw new Error(`Fixture became Rust-unsafe: ${current.category}`)
        }
        return executeNativeRegexBatch(current.plan, fixture.input)
    }

    assertResult('Worker warmup', await runWorker(), fixture.expectedHash)
    assertResult('native warmup', await runNative(), fixture.expectedHash)

    const workerSamples: number[] = []
    const nativeSamples: number[] = []
    for (let index = 0; index < measuredSamples; index++) {
        const first = index % 2 === 0 ? runWorker : runNative
        const second = index % 2 === 0 ? runNative : runWorker
        const firstEngine = index % 2 === 0 ? 'Worker' : 'native'
        const secondEngine = index % 2 === 0 ? 'native' : 'Worker'
        const firstMeasurement = await measure(first)
        const secondMeasurement = await measure(second)
        assertResult(firstEngine, firstMeasurement.result, fixture.expectedHash)
        assertResult(secondEngine, secondMeasurement.result, fixture.expectedHash)
        if (index % 2 === 0) {
            workerSamples.push(firstMeasurement.elapsedMs)
            nativeSamples.push(secondMeasurement.elapsedMs)
        }
        else {
            nativeSamples.push(firstMeasurement.elapsedMs)
            workerSamples.push(secondMeasurement.elapsedMs)
        }
    }

    const workerSummary = summarize(workerSamples)
    const nativeSummary = summarize(nativeSamples)
    const medianImprovement = 1 - nativeSummary.p50Ms / workerSummary.p50Ms
    const p95Improvement = 1 - nativeSummary.p95Ms / workerSummary.p95Ms
    return {
        rules,
        inputBytes: new TextEncoder().encode(fixture.input).byteLength,
        expectedHash: fixture.expectedHash,
        worker: workerSummary,
        native: nativeSummary,
        gate: {
            medianImprovement,
            p95Improvement,
            passed: medianImprovement >= 0.25 && p95Improvement >= 0.20,
        },
    }
}

async function run(): Promise<{
    schemaVersion: 1
    userAgent: string
    cells: PilotCell[]
}> {
    const worker = new RegexWorkerClient()
    const cells: PilotCell[] = []
    for (const rules of ruleCounts) {
        for (const inputTarget of inputTargets) {
            cells.push(await runCell(worker, rules, inputTarget))
        }
    }
    return {
        schemaVersion: 1,
        userAgent: navigator.userAgent,
        cells,
    }
}

declare global {
    interface Window {
        __RISUNEST_REGEX_NATIVE_PILOT__: { run: typeof run }
    }
}

window.__RISUNEST_REGEX_NATIVE_PILOT__ = { run }
document.querySelector('#status')!.textContent = 'ready'
