import assert from 'node:assert/strict'
import test from 'node:test'

import {
    classifyPackageSources,
    decideHighlightGate,
    percentileNearestRank,
    resolveInitialFiles,
} from './bundle-gates.mjs'

test('classifyPackageSources identifies startup packages without requiring every runtime chunk to have sources', () => {
    assert.deepEqual(
        classifyPackageSources([
            '../../node_modules/.pnpm/highlight.js@11/node_modules/highlight.js/es/core.js',
            '../../node_modules/.pnpm/sortablejs@1/node_modules/sortablejs/modular/sortable.core.esm.js',
            '../src/main.ts',
        ]),
        { highlight: true, sortable: true },
    )
})

test('resolveInitialFiles follows only eager imports and includes their CSS', () => {
    const manifest = {
        'index.html': {
            file: 'assets/index.js',
            imports: ['shared'],
            dynamicImports: ['lazy'],
            css: ['assets/index.css'],
        },
        shared: {
            file: 'assets/shared.js',
            css: ['assets/shared.css'],
        },
        lazy: {
            file: 'assets/lazy.js',
            css: ['assets/lazy.css'],
        },
    }

    assert.deepEqual(resolveInitialFiles(manifest, 'index.html'), [
        'assets/index.css',
        'assets/index.js',
        'assets/shared.css',
        'assets/shared.js',
    ])
})

test('resolveInitialFiles rejects missing manifest references', () => {
    assert.throws(
        () => resolveInitialFiles({ 'index.html': { file: 'index.js', imports: ['missing'] } }, 'index.html'),
        /missing manifest entry/i,
    )
})

test('percentileNearestRank returns the observed P95 sample', () => {
    const samples = Array.from({ length: 20 }, (_, index) => index + 1)

    assert.equal(percentileNearestRank(samples, 0.95), 19)
})

test('highlight byte gate rejects 30,719 bytes when the percentage gate also fails', () => {
    const result = decideHighlightGate({
        baselineGzipBytes: 2_000_000,
        candidateGzipBytes: 1_969_281,
        firstUseP95Ms: 99.9,
    })

    assert.equal(result.savedGzipBytes, 30_719)
    assert.equal(result.sizeGatePassed, false)
    assert.equal(result.adopt, false)
})

test('highlight byte gate accepts 30 KiB at 30,720 bytes when P95 is below 100 ms', () => {
    const result = decideHighlightGate({
        baselineGzipBytes: 2_000_000,
        candidateGzipBytes: 1_969_280,
        firstUseP95Ms: 99.9,
    })

    assert.equal(result.savedGzipBytes, 30_720)
    assert.equal(result.sizeGatePassed, true)
    assert.equal(result.adopt, true)
})

test('highlight gate accepts a two percent saving below 30 KiB', () => {
    assert.equal(
        decideHighlightGate({ baselineGzipBytes: 1_000_000, candidateGzipBytes: 980_000, firstUseP95Ms: 99.9 }).adopt,
        true,
    )
})

test('highlight gate rejects a split at the 100 ms boundary', () => {
    assert.equal(
        decideHighlightGate({ baselineGzipBytes: 1_000_000, candidateGzipBytes: 970_000, firstUseP95Ms: 100 }).adopt,
        false,
    )
})
