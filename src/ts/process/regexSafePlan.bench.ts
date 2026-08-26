import { bench } from 'vitest'
import { getRegexExecutionPlan } from './regexExecutionPlan'
import { createRegexWorkerMessageHandler } from './regexWorker'
import type { RegexWorkerResponse } from './regexWorkerClient'
import { makeRegexFixture } from './tests/phase1Fixtures'

const benchOptions = {
    iterations: 11,
    time: 0,
    warmupIterations: 1,
    warmupTime: 0,
}

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

        bench(
            `JavaScript Worker core, ${ruleCount} rules, ${inputBytes / 1024} KiB`,
            () => {
                handle({
                    type: 'execute',
                    id: requestId++,
                    revision: plan.revision,
                    input,
                })
                if (response?.type !== 'result' || response.errors.length !== 0) {
                    throw new Error('JavaScript Worker benchmark failed')
                }
            },
            benchOptions,
        )
    }
}
