import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { maxConcurrentResources, sanitizeMetrics, percentile } from './metrics.mjs'
import {
    addSyntheticAssets,
    assertSyntheticProfile,
    seedExpression,
    syntheticPng,
} from './fixture.mjs'
import { DatabaseSync } from 'node:sqlite'
import { instrumentSource } from './observe.mjs'
import { observeWal } from './host-metrics.mjs'
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { startupInstrumentation } from './android-observation.mjs'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'
import { connectVerifiedIdentity } from './cdp.mjs'

test('CDP retries a disappearing endpoint and still verifies identity before returning', async (t) => {
    const originalWebSocket = globalThis.WebSocket
    t.after(() => {
        globalThis.WebSocket = originalWebSocket
    })
    const sockets = []
    t.mock.method(globalThis, 'fetch', async () => ({
        json: async () => [{ type: 'page', webSocketDebuggerUrl: 'ws://synthetic' }],
    }))
    globalThis.WebSocket = class {
        closed = false
        methods = []
        constructor() {
            this.index = sockets.push(this)
            queueMicrotask(() => (this.index === 1 ? this.onerror() : this.onopen()))
        }
        close() {
            this.closed = true
        }
        send(encoded) {
            const { id, method } = JSON.parse(encoded)
            this.methods.push(method)
            queueMicrotask(() =>
                this.onmessage({
                    data: JSON.stringify({
                        id,
                        result:
                            method === 'Runtime.evaluate'
                                ? { result: { value: this.index === 3 } }
                                : {},
                    }),
                }),
            )
        }
    }
    const client = await connectVerifiedIdentity(12345, 'synthetic', 1000)
    assert.equal(sockets.length, 3)
    assert.equal(sockets[0].closed, true)
    assert.equal(sockets[1].closed, true)
    assert.equal(sockets[2].closed, false)
    assert.ok(
        sockets[2].methods.indexOf('Network.setBlockedURLs') <
            sockets[2].methods.indexOf('Runtime.evaluate'),
    )
    client.close()
    assert.equal(sockets[2].closed, true)
    let failedSocketsClosed = 0
    globalThis.WebSocket = class {
        constructor() {
            queueMicrotask(() => this.onerror())
        }
        close() {
            failedSocketsClosed++
        }
    }
    await assert.rejects(
        connectVerifiedIdentity(12345, 'synthetic', 20),
        /Verified isolated CDP target unavailable/,
    )
    assert.ok(failedSocketsClosed > 0)
})

test('M0 summary rejects a missing per-sample commit even when aggregate count is sufficient', async () => {
    const directory = await mkdtemp(path.join(os.tmpdir(), 'risunest-m0-summary-'))
    const file = path.join(directory, 'result.json')
    try {
        const row = (kind) => ({
            kind,
            warmup: false,
            interaction: {
                success: true,
                failure: null,
                inputAccepted: true,
                scrollImmediateChanged: true,
                inputMs: 1,
                scrollMs: 2,
            },
            stabilizationTimeout: false,
            documentVisible: true,
            calls: [{ command: 'pds_commit', success: true, ms: 3 }],
            longTasks: [],
            usedHeapBytes: 1024,
            host: {
                memory: { appPssBytes: 2048, appRssBytes: 4096 },
                wal: { startBytes: 0, endBytes: 0 },
            },
        })
        const input = {
            platform: 'android-arm64',
            samples: ['reload', 'restart'].flatMap((kind) =>
                Array.from({ length: 20 }, () => row(kind)),
            ),
        }
        const summarize = () =>
            spawnSync(
                process.execPath,
                [fileURLToPath(new URL('./m0-summary.mjs', import.meta.url)), file],
                { encoding: 'utf8', windowsHide: true },
            )
        await writeFile(file, JSON.stringify(input))
        const valid = summarize()
        assert.equal(valid.status, 0, valid.stderr)
        assert.equal(JSON.parse(valid.stdout).groups.reload.samples, 20)
        input.samples[0].documentVisible = false
        await writeFile(file, JSON.stringify(input))
        assert.notEqual(summarize().status, 0)
        input.samples[0].documentVisible = true
        input.samples[1].calls.push(...input.samples[0].calls)
        input.samples[0].calls = []
        await writeFile(file, JSON.stringify(input))
        assert.notEqual(summarize().status, 0)
        input.samples = input.samples.slice(1)
        await writeFile(file, JSON.stringify(input))
        assert.notEqual(summarize().status, 0)
    } finally {
        await rm(directory, { recursive: true, force: true })
    }
})

test('Android observer works with an immutable bridge and preserves promises and errors', async () => {
    const response = { revision: 17 }
    const bridge = Object.freeze({ invoke: async () => response })
    const window = { fetch() {}, __TAURI_INTERNALS__: bridge }
    const run = new Function(
        'window',
        'localStorage',
        'document',
        'PerformanceObserver',
        startupInstrumentation('android'),
    )
    run(
        window,
        { getItem: () => 'false' },
        { addEventListener() {} },
        class {
            observe() {}
        },
    )
    const pending = bridge.invoke('pds_open')
    assert.equal(
        window.__startupObserveCall('pds_open', () => pending),
        pending,
    )
    assert.equal(await pending, response)
    assert.equal(window.__TAURI_INTERNALS__, bridge)
    assert.equal(window.__startupMetrics.firstRevision, 17)
    assert.equal(window.__startupMetrics.calls[0].success, true)
    assert.ok(Number.isFinite(window.__startupMetrics.calls[0].ms))
    const failure = new Error('synthetic failure')
    const rejected = Promise.reject(failure)
    assert.equal(
        window.__startupObserveCall('pds_commit', () => rejected),
        rejected,
    )
    await assert.rejects(rejected, (error) => error === failure)
    assert.equal(window.__startupMetrics.calls[1].success, false)
    assert.throws(
        () =>
            window.__startupObserveCall('pds_commit', () => {
                throw failure
            }),
        (error) => error === failure,
    )
})

test('Android build call sites preserve native error restoration and commit lifetime', async () => {
    const source = `
        async function invokeStore(command, args) {
            try { return await invoke(command, args); } catch (error) { throw restoreStoreError(error); }
        }
        class SqlitePersistentDataStore {
            commit(input) { return transport(input).catch(restoreStoreError); }
        }
    `
    const id = '/src/ts/storage/sqlitePersistentDataStore.ts'
    assert.equal(instrumentSource(source, id, 'windows'), null)
    const output = ts.transpileModule(instrumentSource(source, id, 'android'), {
        compilerOptions: { target: ts.ScriptTarget.ES2022 },
    }).outputText
    const calls = []
    const restored = new Error('restored')
    const pending = Promise.resolve(17)
    const run = new Function(
        'window',
        'invoke',
        'restoreStoreError',
        'transport',
        output + ';return {invokeStore, store: new SqlitePersistentDataStore()};',
    )
    const result = run(
        {
            __startupObserveCall(command, callback) {
                calls.push(command)
                return callback()
            },
        },
        () => Promise.reject('native error'),
        () => restored,
        () => pending,
    )
    await assert.rejects(result.invokeStore('pds_open'), (error) => error === restored)
    assert.equal(await result.store.commit({}), 17)
    assert.deepEqual(calls, ['pds_open', 'pds_commit'])
    const actual = await readFile(
        new URL('../../src/ts/storage/sqlitePersistentDataStore.ts', import.meta.url),
        'utf8',
    )
    assert.ok(instrumentSource(actual, id, 'android').includes('__startupObserveCall'))
})

test('asset counts reject a replacement generation with no aliases', async () => {
    const parent = await mkdtemp(path.join(os.tmpdir(), 'risunest-catalog-test-'))
    const identifier = 'RisuNest.phase3benchmark.r0123456789ab.catalogtest'
    const root = path.join(parent, identifier)
    await mkdir(path.join(root, 'persistent'), { recursive: true })
    const file = path.join(root, 'persistent/persistent.db')
    try {
        const db = new DatabaseSync(file)
        db.exec(`CREATE TABLE meta(key TEXT,value TEXT);
            INSERT INTO meta VALUES('activeGeneration','1');
            CREATE TABLE asset_objects(object_hash TEXT,byte_size INTEGER,created_at_ms INTEGER);
            CREATE TABLE asset_aliases(generation INTEGER,logical_key TEXT,object_hash TEXT,kind TEXT,size INTEGER,mime TEXT,name TEXT,ext TEXT);`)
        db.close()
        const first = await addSyntheticAssets(root, identifier, 2)
        assert.equal(first.objects, 2)
        assert.equal(first.aliases, 2)
        const replaced = new DatabaseSync(file)
        replaced.exec("UPDATE meta SET value='2' WHERE key='activeGeneration'")
        replaced.close()
        await assert.rejects(addSyntheticAssets(root, identifier, 2), /catalog counts/)
    } finally {
        await rm(parent, { recursive: true, force: true })
    }
})

test('WAL observer rejects ordinary profiles and measures only synthetic file sizes', async () => {
    await assert.rejects(observeWal('C:/test/RisuNest', 'RisuNest'))
    const parent = await mkdtemp(path.join(os.tmpdir(), 'risunest-wal-test-'))
    const identifier = 'RisuNest.phase3benchmark.r0123456789ab.waltest'
    const root = path.join(parent, identifier)
    try {
        await mkdir(path.join(root, 'persistent'), { recursive: true })
        const finish = await observeWal(root, identifier)
        try {
            await writeFile(path.join(root, 'persistent/persistent.db-wal'), new Uint8Array(8192))
            const measured = await finish()
            assert.equal(measured.startBytes, 0)
            assert.equal(measured.endBytes, 8192)
            assert.equal(measured.peakBytes, 8192)
            assert.deepEqual(await finish(), measured)
        } finally {
            await finish()
        }
    } finally {
        await rm(parent, { recursive: true, force: true })
    }
})

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
        interaction: {
            selectionMs: 1,
            inputMs: sentinel,
            success: true,
            text: sentinel,
        },
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
