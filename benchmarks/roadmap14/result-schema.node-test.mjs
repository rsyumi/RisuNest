import assert from 'node:assert/strict'
import test from 'node:test'

import { loadRoadmap14ResultSchema, validateRoadmap14Result } from './result-schema.mjs'
import {
    fixtureIdentity,
    getRoadmap14Scenario,
    listRoadmap14Scenarios,
} from './scenarios.mjs'
import {
    createPendingAndroidResults,
    validateAndroidInstrumentationResults,
} from './android.mjs'
import { convertExistingWindowsMeasurements } from './windows.mjs'

test('the shared schema accepts a completed Windows result', () => {
    const saveLarge = getRoadmap14Scenario('save-large')
    const result = {
        schemaVersion: 1,
        kind: 'risunest-roadmap14-platform-result',
        status: 'completed',
        scenario: 'save-large',
        recordedAt: '2026-08-26T00:00:00.000Z',
        fixture: {
            name: saveLarge.name,
            version: saveLarge.version,
            identitySha256: saveLarge.identitySha256,
            descriptor: saveLarge.descriptor,
        },
        build: {
            identity: '742fb370-release',
            sourceRevision: '742fb370',
            profile: 'release',
            target: 'x86_64-pc-windows-msvc',
            appVersion: '1.0.0',
            realmDisabled: true,
        },
        platform: {
            family: 'windows',
            identity: 'windows-test-host',
            osVersion: 'Windows 11',
            architecture: 'x64',
            webViewVersion: 'WebView2 test',
            deviceModel: null,
        },
        memory: {
            measurement: 'js-heap-and-rss',
            heapUsedBytes: [100],
            rssBytes: [200],
            pssBytes: [],
        },
        ui: {
            domNodeCount: 10,
            mountedMessageCount: 2,
            liveUrlCount: 1,
        },
        latency: {
            saveMs: [1],
            importMs: [2],
            exportMs: [3],
            operationMs: [4],
        },
        bytes: {
            fixtureBytes: 1000,
            savedBytes: 900,
            importedBytes: 1000,
            exportedBytes: 900,
        },
        canonicalOutputSha256: 'b'.repeat(64),
        source: {
            runner: 'roadmap14-windows-v1',
            measurements: ['phase3-step5-persistent-store'],
        },
        notes: [],
    }

    assert.deepEqual(validateRoadmap14Result(result), [])
})

test('a completed result cannot silently omit required measurement evidence', () => {
    const result = {
        schemaVersion: 1,
        kind: 'risunest-roadmap14-platform-result',
        status: 'completed',
        scenario: 'library-many',
        recordedAt: '2026-08-26T00:00:00.000Z',
        fixture: {
            name: 'library-many',
            version: 1,
            identitySha256: 'a'.repeat(64),
            descriptor: {},
        },
        build: {
            identity: 'build',
            sourceRevision: null,
            profile: 'release',
            target: 'target',
            appVersion: '1.0.0',
            realmDisabled: true,
        },
        platform: {
            family: 'windows',
            identity: 'host',
            osVersion: 'Windows',
            architecture: 'x64',
            webViewVersion: null,
            deviceModel: null,
        },
        memory: {
            measurement: 'pending',
            heapUsedBytes: [],
            rssBytes: [],
            pssBytes: [],
        },
        ui: {
            domNodeCount: null,
            mountedMessageCount: null,
            liveUrlCount: null,
        },
        latency: {
            saveMs: [],
            importMs: [],
            exportMs: [],
            operationMs: [],
        },
        bytes: {
            fixtureBytes: null,
            savedBytes: null,
            importedBytes: null,
            exportedBytes: null,
        },
        canonicalOutputSha256: null,
        source: { runner: 'test', measurements: [] },
        notes: [],
    }

    const errors = validateRoadmap14Result(result)
    assert.ok(errors.includes('$.canonicalOutputSha256 is required when status is completed'))
    assert.ok(errors.includes('$.ui.domNodeCount is required when status is completed'))
    assert.ok(errors.includes('$.memory requires heap, RSS, or PSS samples when status is completed'))
    assert.ok(errors.includes('$.latency requires at least one sample when status is completed'))
    assert.ok(errors.includes('$.bytes.fixtureBytes is required when status is completed'))
})

test('the checked-in JSON schema requires every cross-platform measurement field', async () => {
    const schema = await loadRoadmap14ResultSchema()

    assert.equal(schema.$id, 'https://risunest.local/schemas/roadmap14-platform-result-v1.json')
    assert.deepEqual(schema.properties.scenario.enum, [
        'library-many',
        'save-large',
        'stream-postprocess',
        'asset-library',
    ])
    assert.deepEqual(schema.required, [
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
    ])

    const errors = validateRoadmap14Result({
        schemaVersion: 1,
        kind: 'risunest-roadmap14-platform-result',
        status: 'pending',
        scenario: 'asset-library',
    })
    assert.ok(errors.includes('$.fixture must be an object'))
    assert.ok(errors.includes('$.memory must be an object'))
    assert.ok(errors.includes('$.ui must be an object'))
    assert.ok(errors.includes('$.latency must be an object'))
    assert.ok(errors.includes('$.bytes must be an object'))
})

test('fixture identity uses stable sorted-key JSON bytes', () => {
    assert.equal(
        fixtureIdentity({ b: 2, a: 1 }),
        '43258cff783fe7036d8a43033f830adfc60ec037382473548ac742b888292777',
    )
})

test('the four synthetic scenario descriptors have deterministic identities', () => {
    const scenarios = listRoadmap14Scenarios()

    assert.deepEqual(scenarios.map(({ name }) => name), [
        'library-many',
        'save-large',
        'stream-postprocess',
        'asset-library',
    ])
    assert.equal(getRoadmap14Scenario('library-many').descriptor.characters, 500)
    assert.equal(getRoadmap14Scenario('save-large').descriptor.stressChat.turns, 10_000)
    assert.equal(getRoadmap14Scenario('stream-postprocess').descriptor.chunks, 512)
    assert.equal(getRoadmap14Scenario('asset-library').descriptor.assets, 10_000)

    assert.deepEqual(
        Object.fromEntries(scenarios.map(({ name, identitySha256 }) => [name, identitySha256])),
        {
            'library-many': 'eb656882a5da2a70b8b48c62daf9bbe8980e808e86bc7933c11a73bcb16da329',
            'save-large': '8479aa2d62405a01fbe69114ebd91d3df75c1448b3017c970ca1b3075c79aca7',
            'stream-postprocess': '51cf472ea0c2c39cdfc3423b4d9900b3c7a06ddd237ad50bd169c472d62fc8fa',
            'asset-library': '7aaa1c5d14e5a065d8bdfbd7ae922d6b415847857c9026625de288b557d27ca3',
        },
    )

    const first = scenarios.map(({ identitySha256 }) => identitySha256)
    const second = listRoadmap14Scenarios().map(({ identitySha256 }) => identitySha256)
    assert.deepEqual(first, second)
    assert.equal(new Set(first).size, 4)
    assert.ok(first.every((identity) => /^[0-9a-f]{64}$/.test(identity)))
})

test('the Android entry point emits schema-valid pending physical-device results', () => {
    const results = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })

    assert.equal(results.length, 4)
    for (const result of results) {
        assert.deepEqual(validateRoadmap14Result(result), [])
        assert.equal(result.status, 'pending')
        assert.equal(result.platform.family, 'android')
        assert.equal(result.platform.identity, 'physical-device-pending')
        assert.equal(result.memory.measurement, 'pending')
        assert.equal(result.canonicalOutputSha256, null)
        assert.deepEqual(result.latency.operationMs, [])
        assert.match(result.notes.join(' '), /physical Android device/i)
    }
})

test('schema validation rejects unknown fields and malformed timestamps', () => {
    const [result] = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })
    result.recordedAt = 'not-a-date'
    result.unexpected = true
    result.memory.unexpected = true

    const errors = validateRoadmap14Result(result)
    assert.ok(errors.includes('$.recordedAt must be an ISO 8601 date-time'))
    assert.ok(errors.includes('$.unexpected is not allowed'))
    assert.ok(errors.includes('$.memory.unexpected is not allowed'))
})

test('schema validation rejects a descriptor that does not match its fixture identity', () => {
    const [result] = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })
    result.fixture.descriptor.characters += 1

    assert.ok(
        validateRoadmap14Result(result).includes(
            '$.fixture.identitySha256 does not match the fixture descriptor',
        ),
    )
})

test('the Android invocation contract rejects non-Android or incomplete scenario sets', () => {
    const results = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })

    assert.deepEqual(validateAndroidInstrumentationResults(results), results)
    assert.throws(
        () => validateAndroidInstrumentationResults(results.slice(1)),
        /exactly one result for each scenario/,
    )
    const wrongPlatform = structuredClone(results)
    wrongPlatform[0].platform.family = 'windows'
    assert.throws(
        () => validateAndroidInstrumentationResults(wrongPlatform),
        /must use platform.family android/,
    )
})

test('the Windows runner converts existing Phase 3 measurements into the shared schema', () => {
    const phase3 = {
        schemaVersion: 1,
        benchmark: 'phase3-step5-persistent-store',
        sourceRevision: '742fb370',
        fixture: { serializedBytes: 1000, sha256: 'c'.repeat(64) },
        samples: [
            {
                importUs: 2000,
                appendCommitUs: 1000,
                exportTotalUs: 3000,
                exportTraversalJsonBytes: 900,
                snapshotUs: 4000,
                snapshotBytes: 1200,
            },
            {
                importUs: 2500,
                appendCommitUs: 1500,
                exportTotalUs: 3500,
                exportTraversalJsonBytes: 900,
                snapshotUs: 4500,
                snapshotBytes: 1200,
            },
        ],
    }
    const tauri = {
        schemaVersion: 1,
        platform: { webViewUserAgent: 'WebView2 test' },
        build: { release: true, realmDisabled: true },
        boot: {
            memory: {
                jsHeap: { usedBytes: 10 },
                processMemory: { workingSetBytes: 20 },
            },
        },
        explicitImport: {
            memorySamples: [
                {
                    jsHeap: { usedBytes: 30 },
                    processMemory: { workingSetBytes: 40 },
                },
            ],
        },
        snapshot: { memorySamples: [] },
        ui: { domNodeCount: 50, mountedMessageCount: 6, liveUrlCount: 2 },
    }

    const result = convertExistingWindowsMeasurements({
        phase3,
        tauri,
        buildIdentity: '742fb370-release',
        platformIdentity: 'windows-reference-host',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
        osVersion: 'Windows 11 test',
        architecture: 'x64',
    })

    assert.deepEqual(validateRoadmap14Result(result), [])
    assert.equal(result.status, 'completed')
    assert.equal(result.canonicalOutputSha256, 'c'.repeat(64))
    assert.deepEqual(result.latency.importMs, [2, 2.5])
    assert.deepEqual(result.latency.saveMs, [1, 1.5])
    assert.deepEqual(result.latency.exportMs, [3, 3.5])
    assert.deepEqual(result.latency.operationMs, [4, 4.5])
    assert.deepEqual(result.memory.heapUsedBytes, [10, 30])
    assert.deepEqual(result.memory.rssBytes, [20, 40])
    assert.deepEqual(result.bytes, {
        fixtureBytes: 1000,
        savedBytes: 1200,
        importedBytes: 1000,
        exportedBytes: 900,
    })
    assert.deepEqual(result.ui, {
        domNodeCount: 50,
        mountedMessageCount: 6,
        liveUrlCount: 2,
    })
})
