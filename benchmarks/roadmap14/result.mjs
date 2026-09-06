import { getRoadmap14Scenario } from './scenarios.mjs'
import { validateRoadmap14Result } from './result-schema.mjs'

export function createRoadmap14Result({
    scenario: scenarioName,
    status,
    recordedAt,
    build,
    platform,
    memory,
    ui,
    latency,
    bytes,
    canonicalOutput,
    source,
    notes = [],
}) {
    const scenario = getRoadmap14Scenario(scenarioName)
    const result = {
        schemaVersion: 2,
        kind: 'risunest-roadmap14-platform-result',
        status,
        scenario: scenario.name,
        recordedAt,
        fixture: {
            name: scenario.name,
            version: scenario.version,
            identitySha256: scenario.identitySha256,
            descriptor: scenario.descriptor,
        },
        build,
        platform,
        memory,
        ui,
        latency,
        bytes,
        canonicalOutput,
        source,
        notes,
    }
    const errors = validateRoadmap14Result(result)
    if (errors.length > 0) {
        throw new Error(`Invalid Roadmap 14 result:\n${errors.join('\n')}`)
    }
    return result
}

