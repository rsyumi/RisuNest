import { readFile } from 'node:fs/promises'

const SHA256_PATTERN = /^[0-9a-f]{64}$/
const SCENARIOS = new Set([
    'library-many',
    'save-large',
    'stream-postprocess',
    'asset-library',
])

const schemaUrl = new URL('./result.schema.json', import.meta.url)

export async function loadRoadmap14ResultSchema() {
    return JSON.parse(await readFile(schemaUrl, 'utf8'))
}

export function validateRoadmap14Result(result) {
    const errors = []

    if (!isRecord(result)) return ['$ must be an object']
    expectAllowedKeys(errors, result, [
        'schemaVersion',
        'kind',
        'status',
        'scenario',
        'recordedAt',
        'fixture',
        'build',
        'platform',
        'memory',
        'ui',
        'latency',
        'bytes',
        'canonicalOutputSha256',
        'source',
        'notes',
    ], '$')
    expectEqual(errors, result.schemaVersion, 1, '$.schemaVersion')
    expectEqual(errors, result.kind, 'risunest-roadmap14-platform-result', '$.kind')
    expectOneOf(errors, result.status, ['completed', 'pending', 'failed'], '$.status')
    expectOneOf(errors, result.scenario, [...SCENARIOS], '$.scenario')
    expectString(errors, result.recordedAt, '$.recordedAt')
    if (typeof result.recordedAt === 'string' && Number.isNaN(Date.parse(result.recordedAt))) {
        errors.push('$.recordedAt must be an ISO 8601 date-time')
    }
    expectRecord(errors, result.fixture, '$.fixture')
    expectRecord(errors, result.build, '$.build')
    expectRecord(errors, result.platform, '$.platform')
    expectRecord(errors, result.memory, '$.memory')
    expectRecord(errors, result.ui, '$.ui')
    expectRecord(errors, result.latency, '$.latency')
    expectRecord(errors, result.bytes, '$.bytes')
    expectRecord(errors, result.source, '$.source')
    expectArray(errors, result.notes, '$.notes')

    if (isRecord(result.fixture)) {
        expectAllowedKeys(
            errors,
            result.fixture,
            ['name', 'version', 'identitySha256', 'descriptor'],
            '$.fixture',
        )
        expectEqual(errors, result.fixture.name, result.scenario, '$.fixture.name')
        expectPositiveInteger(errors, result.fixture.version, '$.fixture.version')
        expectSha256(errors, result.fixture.identitySha256, '$.fixture.identitySha256')
        expectRecord(errors, result.fixture.descriptor, '$.fixture.descriptor')
    }
    if (isRecord(result.build)) {
        expectAllowedKeys(errors, result.build, [
            'identity',
            'sourceRevision',
            'profile',
            'target',
            'appVersion',
            'realmDisabled',
        ], '$.build')
        expectString(errors, result.build.identity, '$.build.identity')
        expectNullableString(errors, result.build.sourceRevision, '$.build.sourceRevision')
        expectOneOf(errors, result.build.profile, ['release', 'profile', 'debug'], '$.build.profile')
        expectString(errors, result.build.target, '$.build.target')
        expectString(errors, result.build.appVersion, '$.build.appVersion')
        expectEqual(errors, result.build.realmDisabled, true, '$.build.realmDisabled')
    }
    if (isRecord(result.platform)) {
        expectAllowedKeys(errors, result.platform, [
            'family',
            'identity',
            'osVersion',
            'architecture',
            'webViewVersion',
            'deviceModel',
        ], '$.platform')
        expectOneOf(errors, result.platform.family, ['windows', 'android'], '$.platform.family')
        expectString(errors, result.platform.identity, '$.platform.identity')
        expectString(errors, result.platform.osVersion, '$.platform.osVersion')
        expectString(errors, result.platform.architecture, '$.platform.architecture')
        expectNullableString(errors, result.platform.webViewVersion, '$.platform.webViewVersion')
        expectNullableString(errors, result.platform.deviceModel, '$.platform.deviceModel')
    }
    if (isRecord(result.memory)) {
        expectAllowedKeys(
            errors,
            result.memory,
            ['measurement', 'heapUsedBytes', 'rssBytes', 'pssBytes'],
            '$.memory',
        )
        expectOneOf(
            errors,
            result.memory.measurement,
            ['js-heap-and-rss', 'android-pss', 'pending'],
            '$.memory.measurement',
        )
        expectNumberArray(errors, result.memory.heapUsedBytes, '$.memory.heapUsedBytes')
        expectNumberArray(errors, result.memory.rssBytes, '$.memory.rssBytes')
        expectNumberArray(errors, result.memory.pssBytes, '$.memory.pssBytes')
    }
    if (isRecord(result.ui)) {
        expectAllowedKeys(
            errors,
            result.ui,
            ['domNodeCount', 'mountedMessageCount', 'liveUrlCount'],
            '$.ui',
        )
        expectNullableNonNegativeInteger(errors, result.ui.domNodeCount, '$.ui.domNodeCount')
        expectNullableNonNegativeInteger(
            errors,
            result.ui.mountedMessageCount,
            '$.ui.mountedMessageCount',
        )
        expectNullableNonNegativeInteger(errors, result.ui.liveUrlCount, '$.ui.liveUrlCount')
    }
    if (isRecord(result.latency)) {
        expectAllowedKeys(
            errors,
            result.latency,
            ['saveMs', 'importMs', 'exportMs', 'operationMs'],
            '$.latency',
        )
        expectNumberArray(errors, result.latency.saveMs, '$.latency.saveMs')
        expectNumberArray(errors, result.latency.importMs, '$.latency.importMs')
        expectNumberArray(errors, result.latency.exportMs, '$.latency.exportMs')
        expectNumberArray(errors, result.latency.operationMs, '$.latency.operationMs')
    }
    if (isRecord(result.bytes)) {
        expectAllowedKeys(
            errors,
            result.bytes,
            ['fixtureBytes', 'savedBytes', 'importedBytes', 'exportedBytes'],
            '$.bytes',
        )
        for (const name of ['fixtureBytes', 'savedBytes', 'importedBytes', 'exportedBytes']) {
            expectNullableNonNegativeInteger(errors, result.bytes[name], `$.bytes.${name}`)
        }
    }
    if (result.canonicalOutputSha256 !== null) {
        expectSha256(errors, result.canonicalOutputSha256, '$.canonicalOutputSha256')
    }
    if (isRecord(result.source)) {
        expectAllowedKeys(errors, result.source, ['runner', 'measurements'], '$.source')
        expectString(errors, result.source.runner, '$.source.runner')
        expectArray(errors, result.source.measurements, '$.source.measurements')
        if (Array.isArray(result.source.measurements)) {
            result.source.measurements.forEach((value, index) =>
                expectString(errors, value, `$.source.measurements[${index}]`))
        }
    }
    if (Array.isArray(result.notes)) {
        result.notes.forEach((value, index) => expectString(errors, value, `$.notes[${index}]`))
    }

    if (result.status === 'completed') {
        if (result.canonicalOutputSha256 === null) {
            errors.push('$.canonicalOutputSha256 is required when status is completed')
        }
        if (isRecord(result.ui)) {
            for (const name of ['domNodeCount', 'mountedMessageCount', 'liveUrlCount']) {
                if (result.ui[name] === null) {
                    errors.push(`$.ui.${name} is required when status is completed`)
                }
            }
        }
        if (isRecord(result.memory)) {
            const memorySampleCount = ['heapUsedBytes', 'rssBytes', 'pssBytes']
                .reduce((total, name) => total + (result.memory[name]?.length ?? 0), 0)
            if (memorySampleCount === 0) {
                errors.push('$.memory requires heap, RSS, or PSS samples when status is completed')
            }
            if (result.memory.measurement === 'pending') {
                errors.push('$.memory.measurement cannot be pending when status is completed')
            }
        }
        if (isRecord(result.latency)) {
            const latencySampleCount = ['saveMs', 'importMs', 'exportMs', 'operationMs']
                .reduce((total, name) => total + (result.latency[name]?.length ?? 0), 0)
            if (latencySampleCount === 0) {
                errors.push('$.latency requires at least one sample when status is completed')
            }
        }
        if (isRecord(result.bytes)) {
            for (const name of ['fixtureBytes', 'savedBytes', 'importedBytes', 'exportedBytes']) {
                if (result.bytes[name] === null) {
                    errors.push(`$.bytes.${name} is required when status is completed`)
                }
            }
        }
    }

    return errors
}

function isRecord(value) {
    return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function expectRecord(errors, value, path) {
    if (!isRecord(value)) errors.push(`${path} must be an object`)
}

function expectArray(errors, value, path) {
    if (!Array.isArray(value)) errors.push(`${path} must be an array`)
}

function expectAllowedKeys(errors, value, allowedKeys, path) {
    const allowed = new Set(allowedKeys)
    for (const key of Object.keys(value)) {
        if (!allowed.has(key)) errors.push(`${path}.${key} is not allowed`)
    }
}

function expectString(errors, value, path) {
    if (typeof value !== 'string' || value.length === 0) errors.push(`${path} must be a non-empty string`)
}

function expectNullableString(errors, value, path) {
    if (value !== null) expectString(errors, value, path)
}

function expectEqual(errors, value, expected, path) {
    if (value !== expected) errors.push(`${path} must equal ${JSON.stringify(expected)}`)
}

function expectOneOf(errors, value, choices, path) {
    if (!choices.includes(value)) errors.push(`${path} must be one of ${choices.join(', ')}`)
}

function expectPositiveInteger(errors, value, path) {
    if (!Number.isSafeInteger(value) || value <= 0) errors.push(`${path} must be a positive integer`)
}

function expectNullableNonNegativeInteger(errors, value, path) {
    if (value !== null && (!Number.isSafeInteger(value) || value < 0)) {
        errors.push(`${path} must be null or a non-negative integer`)
    }
}

function expectNumberArray(errors, value, path) {
    expectArray(errors, value, path)
    if (!Array.isArray(value)) return
    value.forEach((entry, index) => {
        if (typeof entry !== 'number' || !Number.isFinite(entry) || entry < 0) {
            errors.push(`${path}[${index}] must be a non-negative finite number`)
        }
    })
}

function expectSha256(errors, value, path) {
    if (typeof value !== 'string' || !SHA256_PATTERN.test(value)) {
        errors.push(`${path} must be a lowercase SHA-256 hex string`)
    }
}
