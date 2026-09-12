import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { maxConcurrentResources, sanitizeMetrics, percentile } from './metrics.mjs'
import { assertSyntheticProfile, seedExpression, syntheticPng } from './fixture.mjs'
import { instrumentSource } from './observe.mjs'

test('refuses ordinary and mismatched profiles before fixture IO', () => {
    assert.throws(() => assertSyntheticProfile('C:/test/RisuNest', 'RisuNest'))
    assert.throws(() =>
        assertSyntheticProfile(
            'C:/test/RisuNest',
            'RisuNest.phase3benchmark.r0123456789ab.fixture',
        ),
    )
    assert.doesNotThrow(() =>
        assertSyntheticProfile(
            'C:/test/RisuNest.phase3benchmark.r0123456789ab.fixture',
            'RisuNest.phase3benchmark.r0123456789ab.fixture',
        ),
    )
})

test('allowlisted projection removes body, path, identifier, and exception sentinels', () => {
    const sentinel = '__synthetic_private_sentinel__'
    const output = sanitizeMetrics({
        interactiveMs: 10,
        body: sentinel,
        firstRevision: sentinel,
        calls: [
            { command: sentinel, ms: 1 },
            { command: 'pds_open', ms: 2, path: sentinel, bytes: 15, success: true },
        ],
        operations: [{ stage: 'canonical', ms: 3, body: sentinel }],
        phases: [{ stage: sentinel, ms: 1 }],
        active: { [sentinel]: 1 },
        interaction: { selectionMs: 1, inputMs: sentinel, success: true, text: sentinel },
    })
    assert.equal(JSON.stringify(output).includes(sentinel), false)
    assert.equal(output.calls.length, 1)
    assert.equal(output.calls[0].bytes, 15)
    assert.equal(output.interaction.inputMs, null)
})

test('nearest-rank P95 uses the nineteenth of twenty samples', () => {
    assert.equal(
        percentile(
            Array.from({ length: 20 }, (_, i) => i + 1),
            95,
        ),
        19,
    )
    assert.equal(percentile([], 95), null)
})

test('resource concurrency counts overlap without merging adjacent requests', () => {
    assert.equal(
        maxConcurrentResources([
            { start: 0, ms: 10 },
            { start: 5, ms: 10 },
        ]),
        2,
    )
    assert.equal(
        maxConcurrentResources([
            { start: 0, ms: 10 },
            { start: 10, ms: 10 },
        ]),
        1,
    )
    assert.equal(
        maxConcurrentResources([
            { start: 0, ms: 0 },
            { start: 1, ms: null },
        ]),
        0,
    )
})

test('synthetic PNG samples have distinct contents and real image dimensions', () => {
    const first = syntheticPng(0),
        second = syntheticPng(1)
    assert.notDeepEqual(first, second)
    assert.equal(first.readUInt32BE(16), 256)
    assert.equal(first.readUInt32BE(20), 256)
})

test('synthetic seed supplies ordered selectable characters and exact body references', async () => {
    let root
    const characters = []
    const invoke = async (command, args) => {
        if (command === 'pds_materialize') return { characters: [], botPresets: [] }
        if (command === 'pds_replace_begin') return { stagingId: 'synthetic-staging' }
        if (command === 'pds_replace_put_root') root = args.root
        if (command === 'pds_replace_add_characters') characters.push(...args.characters)
        if (command === 'pds_replace_preserve_repositories') return { revision: 1 }
    }
    const expression = seedExpression({
        characters: 3,
        bytes: 4096,
        pluginBytes: 32,
        referencedAssets: 10,
    })
    const result = await new Function('window', 'localStorage', `return ${expression}`)(
        { __TAURI_INTERNALS__: { invoke } },
        { setItem() {} },
    )
    assert.equal(result.success, true)
    assert.deepEqual(
        root.characterOrder,
        characters.map((c) => c.chaId),
    )
    assert.ok(characters.every((c) => Array.isArray(c.globalLore) && c.chats.length > 0))
    const references = characters.flatMap((c) => c.additionalAssets.map((a) => a[1]))
    assert.equal(references.length, 10)
    assert.equal(new Set(references).size, 10)
})

test('every measured source boundary exists in the current checkout', async () => {
    for (const file of [
        'storage/databasePreparation.ts',
        'storage/saveCoordinatorHelpers.ts',
        'storage/saveCoordinator.ts',
        'bootstrap.ts',
        'globalApi.svelte.ts',
    ]) {
        const source = await readFile(new URL('../../src/ts/' + file, import.meta.url), 'utf8')
        const transformed = instrumentSource(source, '/src/ts/' + file)
        assert.ok(
            transformed.includes('__startupRecord') ||
                transformed.includes('__startupTrackPromise'),
        )
    }
})
