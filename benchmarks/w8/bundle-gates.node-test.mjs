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

test('highlight gate accepts either required gzip saving when P95 is below 100 ms', () => {
    assert.deepEqual(
        decideHighlightGate({ baselineGzipBytes: 1_000_000, candidateGzipBytes: 970_000, firstUseP95Ms: 99.9 }),
        {
            adopt: true,
            savedGzipBytes: 30_000,
            savedPercent: 3,
            sizeGatePassed: true,
            latencyGatePassed: true,
        },
    )
})

test('highlight gate rejects a split at the 100 ms boundary', () => {
    assert.equal(
        decideHighlightGate({ baselineGzipBytes: 1_000_000, candidateGzipBytes: 970_000, firstUseP95Ms: 100 }).adopt,
        false,
    )
})
