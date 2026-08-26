import { readFileSync } from 'node:fs'

import { fixtureIdentity, getRoadmap14Scenario } from './scenarios.mjs'

const RESULT_SCHEMA = JSON.parse(
    readFileSync(new URL('./result.schema.json', import.meta.url), 'utf8'),
)

export async function loadRoadmap14ResultSchema() {
    return structuredClone(RESULT_SCHEMA)
}

export function validateRoadmap14JsonSchema(result) {
    return validateSchemaValue(result, RESULT_SCHEMA, '$', RESULT_SCHEMA)
}

export function validateRoadmap14Result(result) {
    const errors = validateRoadmap14JsonSchema(result)
    if (!isRecord(result)) return errors

    if (isRecord(result.fixture) && typeof result.scenario === 'string') {
        if (result.fixture.name !== result.scenario) {
            errors.push('$.fixture.name must equal $.scenario')
        }
        if (isRecord(result.fixture.descriptor)) {
            const actualIdentity = fixtureIdentity({
                name: result.fixture.name,
                version: result.fixture.version,
                descriptor: result.fixture.descriptor,
            })
            if (result.fixture.identitySha256 !== actualIdentity) {
                errors.push('$.fixture.identitySha256 does not match the fixture descriptor')
            }
            try {
                const frozen = getRoadmap14Scenario(result.scenario)
                if (
                    result.fixture.version !== frozen.version
                    || actualIdentity !== frozen.identitySha256
                ) {
                    errors.push('$.fixture must match the frozen scenario definition')
                }
            } catch {
                // The JSON Schema reports unknown scenario names.
            }
        }
    }

    requireUniqueNames(errors, result.latency?.samples, '$.latency.samples')
    requireUniqueNames(errors, result.bytes?.artifacts, '$.bytes.artifacts')
    requireUniqueNames(errors, result.source?.artifacts, '$.source.artifacts')

    return errors
}

function validateSchemaValue(value, schema, path, rootSchema) {
    if (schema === true) return []
    if (schema === false) return [`${path} is not allowed`]
    if (!isRecord(schema)) return [`${path} has an invalid schema`]

    const errors = []
    if (typeof schema.$ref === 'string') {
        errors.push(...validateSchemaValue(value, resolveReference(rootSchema, schema.$ref), path, rootSchema))
    }
    if (Array.isArray(schema.allOf)) {
        for (const branch of schema.allOf) {
            errors.push(...validateSchemaValue(value, branch, path, rootSchema))
        }
    }
    if (Array.isArray(schema.anyOf)) {
        const matches = schema.anyOf.some(
            (branch) => validateSchemaValue(value, branch, path, rootSchema).length === 0,
        )
        if (!matches) errors.push(`${path} must match at least one allowed schema`)
    }
    if (isRecord(schema.if)) {
        const conditionMatches = validateSchemaValue(value, schema.if, path, rootSchema).length === 0
        if (conditionMatches && isRecord(schema.then)) {
            errors.push(...validateSchemaValue(value, schema.then, path, rootSchema))
        } else if (!conditionMatches && isRecord(schema.else)) {
            errors.push(...validateSchemaValue(value, schema.else, path, rootSchema))
        }
    }

    if ('const' in schema && !deepEqual(value, schema.const)) {
        errors.push(`${path} must equal ${JSON.stringify(schema.const)}`)
    }
    if (Array.isArray(schema.enum) && !schema.enum.some((entry) => deepEqual(value, entry))) {
        errors.push(`${path} must be one of ${schema.enum.join(', ')}`)
    }
    if (schema.type !== undefined && !matchesType(value, schema.type)) {
        errors.push(`${path} must be ${describeType(schema.type)}`)
        return errors
    }

    if (isRecord(value)) {
        const properties = isRecord(schema.properties) ? schema.properties : {}
        if (Array.isArray(schema.required)) {
            for (const name of schema.required) {
                if (!(name in value)) errors.push(`${path}.${name} is required`)
            }
        }
        if (schema.additionalProperties === false) {
            for (const name of Object.keys(value)) {
                if (!(name in properties)) errors.push(`${path}.${name} is not allowed`)
            }
        }
        for (const [name, propertySchema] of Object.entries(properties)) {
            if (name in value) {
                errors.push(...validateSchemaValue(value[name], propertySchema, `${path}.${name}`, rootSchema))
            }
        }
    }

    if (Array.isArray(value)) {
        if (Number.isSafeInteger(schema.minItems) && value.length < schema.minItems) {
            errors.push(`${path} must contain at least ${schema.minItems} item(s)`)
        }
        if (schema.items !== undefined) {
            value.forEach((entry, index) => {
                errors.push(...validateSchemaValue(entry, schema.items, `${path}[${index}]`, rootSchema))
            })
        }
    }

    if (typeof value === 'string') {
        if (Number.isSafeInteger(schema.minLength) && value.length < schema.minLength) {
            errors.push(`${path} must contain at least ${schema.minLength} character(s)`)
        }
        if (typeof schema.pattern === 'string' && !new RegExp(schema.pattern).test(value)) {
            errors.push(`${path} must match ${schema.pattern}`)
        }
        if (schema.format === 'date-time' && Number.isNaN(Date.parse(value))) {
            errors.push(`${path} must be an ISO 8601 date-time`)
        }
    }

    if (typeof value === 'number' && Number.isFinite(schema.minimum) && value < schema.minimum) {
        errors.push(`${path} must be at least ${schema.minimum}`)
    }

    return errors
}

function resolveReference(rootSchema, reference) {
    if (!reference.startsWith('#/')) throw new Error(`Unsupported JSON Schema reference: ${reference}`)
    return reference.slice(2).split('/').reduce((value, segment) => {
        const key = segment.replaceAll('~1', '/').replaceAll('~0', '~')
        return value[key]
    }, rootSchema)
}

function matchesType(value, type) {
    const types = Array.isArray(type) ? type : [type]
    return types.some((candidate) => {
        if (candidate === 'null') return value === null
        if (candidate === 'array') return Array.isArray(value)
        if (candidate === 'object') return isRecord(value)
        if (candidate === 'integer') return Number.isSafeInteger(value)
        if (candidate === 'number') return typeof value === 'number' && Number.isFinite(value)
        return typeof value === candidate
    })
}

function describeType(type) {
    return (Array.isArray(type) ? type : [type]).join(' or ')
}

function requireUniqueNames(errors, entries, path) {
    if (!Array.isArray(entries)) return
    const names = entries.map((entry) => entry?.name).filter((name) => typeof name === 'string')
    if (new Set(names).size !== names.length) errors.push(`${path} names must be unique`)
}

function isRecord(value) {
    return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function deepEqual(left, right) {
    if (Object.is(left, right)) return true
    if (Array.isArray(left) && Array.isArray(right)) {
        return left.length === right.length && left.every((value, index) => deepEqual(value, right[index]))
    }
    if (isRecord(left) && isRecord(right)) {
        const leftKeys = Object.keys(left)
        const rightKeys = Object.keys(right)
        return leftKeys.length === rightKeys.length
            && leftKeys.every((key) => key in right && deepEqual(left[key], right[key]))
    }
    return false
}
