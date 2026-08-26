import assert from 'node:assert/strict'
import test from 'node:test'

import {
    REQUIRED_COMPATIBILITY_CLASSES,
    createOracleBenchmarkFixtures,
    loadTokenizerCorpus,
    percentile,
    verifyArtifactHashes,
    verifyTokenizerCorpus,
} from './corpus.mjs'

test('the checked-in artifacts match the literal corpus provenance', async () => {
    const corpus = await loadTokenizerCorpus()

    assert.deepEqual(await verifyArtifactHashes(corpus), {
        cl100k_base: corpus.artifacts.cl100k_base.sha256,
        o200k_base: corpus.artifacts.o200k_base.sha256,
    })
})

test('both JavaScript oracle artifacts match every literal token ID and error', async () => {
    const corpus = await loadTokenizerCorpus()

    assert.deepEqual(await verifyTokenizerCorpus(corpus), {
        cl100k_base: { idCases: 34, errorCases: 10 },
        o200k_base: { idCases: 34, errorCases: 4 },
    })
})

test('the retained corpus covers every Roadmap 14 compatibility class', async () => {
    const corpus = await loadTokenizerCorpus()
    const classes = new Set(corpus.cases.map((entry) => entry.class))

    for (const requiredClass of REQUIRED_COMPATIBILITY_CLASSES) {
        assert.ok(classes.has(requiredClass), `missing compatibility class: ${requiredClass}`)
    }
})

test('oracle benchmark fixtures retain ordered batch and long-input shapes', async () => {
    const corpus = await loadTokenizerCorpus()
    const fixtures = createOracleBenchmarkFixtures(corpus, 'cl100k_base')

    assert.deepEqual(
        fixtures.map((fixture) => [fixture.name, fixture.texts.length]),
        [
            ['short-segments-1', 1],
            ['short-segments-10', 10],
            ['short-segments-100', 100],
            ['short-segments-1000', 1000],
            ['prompt-over-32-kib', 1],
            ['realistic-prompt-segments', 6],
        ],
    )
    assert.ok(Buffer.byteLength(fixtures[4].texts[0], 'utf8') >= 32 * 1024)
})

test('percentile uses nearest rank without mutating samples', () => {
    const samples = [9, 1, 5, 3]

    assert.equal(percentile(samples, 0.5), 3)
    assert.equal(percentile(samples, 0.95), 9)
    assert.deepEqual(samples, [9, 1, 5, 3])
})
