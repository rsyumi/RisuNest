import { describe, expect, it } from 'vitest'
import { getRegexExecutionPlan } from './regexExecutionPlan'
import { createRegexWorkerMessageHandler } from './regexWorker'
import type { RegexWorkerResponse } from './regexWorkerClient'
import { makeRegexFixture } from './tests/phase1Fixtures'

describe.skipIf(process.env.RISUNEST_REGEX_PROFILE !== 'true')(
    'Windows regex shadow profile',
    () => {
        it('records JavaScript Worker core P50 and P95', () => {
            for (const ruleCount of [20, 100, 500] as const) {
                for (const inputBytes of [32 * 1024, 256 * 1024, 1024 * 1024]) {
                    const fixture = makeRegexFixture(ruleCount, inputBytes)
                    const input = fixture.input.slice(0, inputBytes)
                    const plan = getRegexExecutionPlan(fixture.scripts, 'editoutput')
                    let response: RegexWorkerResponse | undefined
                    let requestId = 0
                    const handle = createRegexWorkerMessageHandler((message) => {
                        response = message
                    })
                    handle({
                        type: 'register',
                        revision: plan.revision,
                        entries: plan.entries.map((entry) => [
                            entry.sourceIndex,
                            entry.pattern,
                            entry.replacement,
                            entry.flags,
                        ]),
                    })
                    const samples: number[] = []
                    for (let run = 0; run < 11; run++) {
                        const started = performance.now()
                        handle({
                            type: 'execute',
                            id: requestId++,
                            revision: plan.revision,
                            input,
                        })
                        const elapsed = performance.now() - started
                        expect(response).toMatchObject({ type: 'result', errors: [] })
                        if (run !== 0) {
                            samples.push(elapsed)
                        }
                    }
                    samples.sort((left, right) => left - right)
                    console.log(JSON.stringify({
                        engine: 'javascript_worker_core',
                        rules: ruleCount,
                        inputBytes,
                        samples: samples.length,
                        p50Micros: Math.round(((samples[4] + samples[5]) / 2) * 1_000),
                        p95Micros: Math.round(samples[9] * 1_000),
                    }))
                }
            }
        }, 60_000)
    },
)
