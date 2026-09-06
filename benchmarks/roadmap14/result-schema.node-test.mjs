import assert from 'node:assert/strict'
import test from 'node:test'

import {
    loadRoadmap14ResultSchema,
    validateRoadmap14JsonSchema,
    validateRoadmap14Result,
} from './result-schema.mjs'
import {
    fixtureIdentity,
    getRoadmap14Scenario,
    listRoadmap14Scenarios,
} from './scenarios.mjs'
import {
    createPendingAndroidResults,
    validateAndroidInstrumentationResults,
} from './android.mjs'
import { convertExistingWindowsMeasurements, parseArguments as parseWindowsArguments } from './windows.mjs'

test('the version 2 contract names measured operations and canonical output provenance', async () => {
    const [result] = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })
    const schema = await loadRoadmap14ResultSchema()

    assert.equal(schema.$id, 'https://risunest.local/schemas/roadmap14-platform-result-v2.json')
    assert.equal(result.schemaVersion, 2)
    assert.deepEqual(result.latency, { samples: [] })
    assert.deepEqual(result.bytes, { artifacts: [] })
    assert.equal(result.canonicalOutput, null)
    assert.deepEqual(result.source.artifacts, [])
})

function completedAndroidResults() {
    return createPendingAndroidResults({
        sourceRevision: '742fb370742fb370742fb370742fb370742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    }).map((result) => ({
        ...result,
        status: 'completed',
        build: {
            ...result.build,
            identity: '742fb370742fb370742fb370742fb370742fb370-android-release',
        },
        platform: {
            ...result.platform,
            identity: 'pixel-8-pro-serial-1234',
            osVersion: 'Android 16',
            architecture: 'arm64-v8a',
            webViewVersion: 'WebView 151',
            deviceModel: 'Pixel 8 Pro',
        },
        memory: {
            measurement: 'android-pss',
            heapUsedBytes: [],
            rssBytes: [],
            pssBytes: [100],
        },
        ui: {
            domNodeCount: 10,
            mountedMessageCount: 2,
            liveUrlCount: 1,
        },
        latency: {
            samples: [{ name: 'scenario-operation', valuesMs: [1] }],
        },
        bytes: {
            artifacts: [{ name: 'scenario-output', bytes: 900 }],
        },
        canonicalOutput: {
            sha256: 'd'.repeat(64),
            provenance: 'physical-device-instrumentation-output',
        },
        source: {
            runner: 'roadmap14-android-instrumentation-v2',
            measurements: ['physical-device-instrumentation'],
            artifacts: [{ name: 'instrumentation-result', sha256: 'e'.repeat(64) }],
        },
    }))
}

test('the shared schema accepts a completed Windows result', () => {
    const saveLarge = getRoadmap14Scenario('save-large')
    const result = {
        schemaVersion: 2,
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
            sourceRevision: '742fb370742fb370742fb370742fb370742fb370',
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
            samples: [
                { name: 'staged-replace-import', valuesMs: [2] },
                { name: 'append-commit', valuesMs: [1] },
                { name: 'export-traversal', valuesMs: [3] },
                { name: 'snapshot-create', valuesMs: [4] },
            ],
        },
        bytes: {
            artifacts: [
                { name: 'fixture-serialized-json', bytes: 1000 },
                { name: 'export-traversal-json', bytes: 900 },
            ],
        },
        canonicalOutput: {
            sha256: 'b'.repeat(64),
            provenance: 'phase3-export-traversal-after-append',
        },
        source: {
            runner: 'roadmap14-windows-v2',
            measurements: ['phase3-step5-persistent-store'],
            artifacts: [{ name: 'phase3-result', sha256: 'c'.repeat(64) }],
        },
        notes: [],
    }

    assert.deepEqual(validateRoadmap14Result(result), [])
})

test('a completed result cannot silently omit required measurement evidence', () => {
    const result = {
        schemaVersion: 2,
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
            samples: [],
        },
        bytes: {
            artifacts: [],
        },
        canonicalOutput: null,
        source: { runner: 'test', measurements: [], artifacts: [] },
        notes: [],
    }

    const errors = validateRoadmap14Result(result)
    assert.ok(errors.includes('$.canonicalOutput must be object'))
    assert.ok(errors.includes('$.ui.domNodeCount must be integer'))
    assert.ok(errors.includes('$.memory must match at least one allowed schema'))
    assert.ok(errors.includes('$.latency.samples must contain at least 1 item(s)'))
    assert.ok(errors.includes('$.bytes.artifacts must contain at least 1 item(s)'))
})

test('the checked-in JSON schema requires every cross-platform measurement field', async () => {
    const schema = await loadRoadmap14ResultSchema()

    assert.equal(schema.$id, 'https://risunest.local/schemas/roadmap14-platform-result-v2.json')
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
        'canonicalOutput',
        'source',
        'notes',
    ])

    const errors = validateRoadmap14Result({
        schemaVersion: 2,
        kind: 'risunest-roadmap14-platform-result',
        status: 'pending',
        scenario: 'asset-library',
    })
    assert.ok(errors.includes('$.fixture is required'))
    assert.ok(errors.includes('$.memory is required'))
    assert.ok(errors.includes('$.ui is required'))
    assert.ok(errors.includes('$.latency is required'))
    assert.ok(errors.includes('$.bytes is required'))
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
        assert.equal(result.canonicalOutput, null)
        assert.deepEqual(result.latency.samples, [])
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

test('runtime result validation is structurally driven by the checked-in JSON Schema', () => {
    const [result] = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })

    assert.deepEqual(validateRoadmap14JsonSchema(result), [])
    assert.deepEqual(validateRoadmap14Result(result), [])

    result.source.artifacts = [{ name: 'bad-hash', sha256: 'not-a-hash' }]
    assert.ok(validateRoadmap14JsonSchema(result).length > 0)
    assert.ok(validateRoadmap14Result(result).length > 0)
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

test('schema validation rejects a self-consistent fixture that differs from the frozen scenario', () => {
    const [result] = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })
    result.fixture.descriptor.characters += 1
    result.fixture.identitySha256 = fixtureIdentity({
        name: result.fixture.name,
        version: result.fixture.version,
        descriptor: result.fixture.descriptor,
    })

    assert.ok(
        validateRoadmap14Result(result).includes(
            '$.fixture must match the frozen scenario definition',
        ),
    )
})

test('the Android instrumentation contract accepts only completed submissions', () => {
    const pending = createPendingAndroidResults({
        sourceRevision: '742fb370',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
    })

    assert.throws(
        () => validateAndroidInstrumentationResults(pending),
        /must have status completed/,
    )

    const completed = completedAndroidResults()
    assert.deepEqual(validateAndroidInstrumentationResults(completed), completed)
})

test('the Android instrumentation contract rejects emulator evidence', () => {
    const results = completedAndroidResults()
    results.forEach((result) => {
        result.platform.identity = 'emulator-5554'
        result.platform.deviceModel = 'sdk_gphone64_x86_64'
    })

    assert.throws(
        () => validateAndroidInstrumentationResults(results),
        /must identify a physical device/,
    )
})

test('the Android instrumentation contract requires one physical device', () => {
    const results = completedAndroidResults()
    results[1].platform.identity = 'pixel-9-pro-serial-5678'
    results[1].platform.deviceModel = 'Pixel 9 Pro'

    assert.throws(
        () => validateAndroidInstrumentationResults(results),
        /must come from one physical device/,
    )
})

test('the Android instrumentation contract requires one build revision', () => {
    const results = completedAndroidResults()
    results[2].build.sourceRevision = 'abcdefabcdefabcdefabcdefabcdefabcdefabcd'

    assert.throws(
        () => validateAndroidInstrumentationResults(results),
        /must use one build and source revision/,
    )
})

test('the Android invocation contract rejects non-Android or incomplete scenario sets', () => {
    const results = completedAndroidResults()

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
    const sourceRevision = '742fb370742fb370742fb370742fb370742fb370'
    const phase3 = {
        schemaVersion: 1,
        benchmark: 'phase3-step5-persistent-store',
        sourceRevision,
        fixture: {
            characters: 500,
            totalConversations: 5001,
            totalMessages: 510000,
            serializedBytes: 1000,
            sha256: 'c'.repeat(64),
        },
        exportTraversalSha256Provenance: 'sha256-of-u64le-length-prefixed-json-fragments-in-export-traversal-order-after-append',
        samples: [
            {
                importUs: 2000,
                appendCommitUs: 1000,
                exportTotalUs: 3000,
                exportTraversalJsonBytes: 900,
                exportTraversalSha256: 'd'.repeat(64),
                snapshotUs: 4000,
                snapshotBytes: 1200,
            },
            {
                importUs: 2500,
                appendCommitUs: 1500,
                exportTotalUs: 3500,
                exportTraversalJsonBytes: 900,
                exportTraversalSha256: 'd'.repeat(64),
                snapshotUs: 4500,
                snapshotBytes: 1200,
            },
        ],
    }
    const tauri = {
        schemaVersion: 1,
        platform: { webViewUserAgent: 'WebView2 test' },
        build: {
            release: true,
            realmDisabled: true,
            sourceRevision,
            identifier: 'RisuNest.phase3benchmark.r742fb370742f.run123',
        },
        fixture: {
            kind: 'phase3-step5-save-large',
            serializedBytes: 1000,
            serializedSha256: 'c'.repeat(64),
            characters: 500,
            totalConversations: 5001,
            totalMessages: 510000,
        },
        boot: {
            memory: {
                jsHeap: { usedBytes: 10 },
                processMemory: { workingSetBytes: 20 },
            },
        },
        explicitImport: {
            memorySamples: [
                {
                    jsHeap: { usedBytes: 10 },
                    processMemory: { workingSetBytes: 20 },
                },
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
        sourceRevision,
        artifactHashes: {
            phase3Result: 'e'.repeat(64),
            tauriResult: 'f'.repeat(64),
        },
        platformIdentity: 'windows-reference-host',
        appVersion: '1.0.0',
        recordedAt: '2026-08-26T00:00:00.000Z',
        osVersion: 'Windows 11 test',
        architecture: 'x64',
    })

    assert.deepEqual(validateRoadmap14Result(result), [])
    assert.equal(result.status, 'completed')
    assert.equal(result.build.identity, `${sourceRevision}-windows-release`)
    assert.deepEqual(result.canonicalOutput, {
        sha256: 'd'.repeat(64),
        provenance: 'sha256-of-u64le-length-prefixed-json-fragments-in-export-traversal-order-after-append',
    })
    assert.deepEqual(result.latency.samples, [
        { name: 'staged-replace-import', valuesMs: [2, 2.5] },
        { name: 'append-commit', valuesMs: [1, 1.5] },
        { name: 'export-materialize-and-traversal-total', valuesMs: [3, 3.5] },
        { name: 'snapshot-create', valuesMs: [4, 4.5] },
    ])
    assert.deepEqual(result.memory.heapUsedBytes, [10, 30])
    assert.deepEqual(result.memory.rssBytes, [20, 40])
    assert.deepEqual(result.bytes, {
        artifacts: [
            { name: 'fixture-serialized-json', bytes: 1000 },
            { name: 'export-traversal-json-fragments', bytes: 900 },
            { name: 'snapshot-file', bytes: 1200 },
        ],
    })
    assert.deepEqual(result.ui, {
        domNodeCount: 50,
        mountedMessageCount: 6,
        liveUrlCount: 2,
    })
    assert.deepEqual(result.source.artifacts, [
        { name: 'save-large-fixture-json', sha256: 'c'.repeat(64) },
        { name: 'phase3-result-json', sha256: 'e'.repeat(64) },
        { name: 'tauri-cdp-result-json', sha256: 'f'.repeat(64) },
    ])

    assert.throws(
        () => convertExistingWindowsMeasurements({
            phase3,
            tauri: {
                ...tauri,
                build: { ...tauri.build, sourceRevision: 'different-revision' },
            },
            sourceRevision,
            artifactHashes: {
                phase3Result: 'e'.repeat(64),
                tauriResult: 'f'.repeat(64),
            },
            platformIdentity: 'windows-reference-host',
            appVersion: '1.0.0',
            recordedAt: '2026-08-26T00:00:00.000Z',
            osVersion: 'Windows 11 test',
            architecture: 'x64',
        }),
        /must match Git HEAD/,
    )
    assert.throws(
        () => convertExistingWindowsMeasurements({
            phase3,
            tauri: {
                ...tauri,
                fixture: { ...tauri.fixture, serializedSha256: '0'.repeat(64) },
            },
            sourceRevision,
            artifactHashes: {
                phase3Result: 'e'.repeat(64),
                tauriResult: 'f'.repeat(64),
            },
            platformIdentity: 'windows-reference-host',
            appVersion: '1.0.0',
            recordedAt: '2026-08-26T00:00:00.000Z',
            osVersion: 'Windows 11 test',
            architecture: 'x64',
        }),
        /same frozen save-large fixture/,
    )
})

test('the Windows live runner does not accept build or hash provenance overrides', () => {
    assert.throws(
        () => parseWindowsArguments(['--build-identity', 'spoofed']),
        /Unknown argument: --build-identity/,
    )
    assert.throws(
        () => parseWindowsArguments(['--canonical-output-sha256', 'a'.repeat(64)]),
        /Unknown argument: --canonical-output-sha256/,
    )
})
