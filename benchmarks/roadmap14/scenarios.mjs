import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'

const definitions = JSON.parse(
    readFileSync(new URL('./scenarios.json', import.meta.url), 'utf8'),
)

export function fixtureIdentity(value) {
    return createHash('sha256').update(stableJson(value)).digest('hex')
}

export function listRoadmap14Scenarios() {
    return definitions.map(toScenario)
}

export function getRoadmap14Scenario(name) {
    const definition = definitions.find((candidate) => candidate.name === name)
    if (!definition) throw new Error(`Unknown Roadmap 14 scenario: ${name}`)
    return toScenario(definition)
}

function toScenario(definition) {
    const value = structuredClone(definition)
    return {
        ...value,
        identitySha256: fixtureIdentity(value),
    }
}

function stableJson(value) {
    if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`
    if (value !== null && typeof value === 'object') {
        return `{${Object.keys(value)
            .sort()
            .map((key) => `${JSON.stringify(key)}:${stableJson(value[key])}`)
            .join(',')}}`
    }
    if (typeof value === 'number' && !Number.isFinite(value)) {
        throw new Error('Fixture identity does not support non-finite numbers')
    }
    const encoded = JSON.stringify(value)
    if (encoded === undefined) throw new Error('Fixture identity does not support undefined')
    return encoded
}
