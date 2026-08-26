import { writeFile } from 'node:fs/promises'
import path from 'node:path'
import process from 'node:process'
import { performance } from 'node:perf_hooks'
import { fileURLToPath } from 'node:url'

import {
    createOracleBenchmarkFixtures,
    createOracleEncoder,
    loadTokenizerCorpus,
    percentile,
    verifyArtifactHashes,
    verifyTokenizerCorpus,
} from './corpus.mjs'

const TOKENIZER_IDS = ['cl100k_base', 'o200k_base']
const MODES = ['count', 'ids']

function requiredValue(argumentsList, index, option) {
    const value = argumentsList[index]
    if (!value || value.startsWith('--')) throw new Error(`${option} requires a value`)
    return value
}

export function parseArguments(argumentsList) {
    const options = { samples: 20, output: null }
    for (let index = 0; index < argumentsList.length; index++) {
        const argument = argumentsList[index]
        if (argument === '--samples') {
            const value = Number(requiredValue(argumentsList, ++index, argument))
            if (!Number.isSafeInteger(value) || value <= 0) {
                throw new Error('--samples requires a positive integer')
            }
            if (value > 1000) throw new Error('--samples must be at most 1000')
            options.samples = value
        } else if (argument === '--output') {
            options.output = requiredValue(argumentsList, ++index, argument)
        } else {
            throw new Error(`Unknown argument: ${argument}`)
        }
    }
    return options
}

function checksumValue(checksum, value) {
    return Math.imul(checksum ^ value, 16_777_619) >>> 0
}

function encodeBatch(tokenizer, texts, mode) {
    const ids = texts.map((text) => tokenizer.encode(text))
    if (mode === 'count') return ids.map((entry) => entry.length)
    return ids
}

function summarizeOutput(output, mode) {
    let checksum = 2_166_136_261
    let totalIds = 0
    if (mode === 'count') {
        for (const count of output) {
            totalIds += count
            checksum = checksumValue(checksum, count)
        }
    } else {
        for (const ids of output) {
            totalIds += ids.length
            checksum = checksumValue(checksum, ids.length)
            for (const id of ids) checksum = checksumValue(checksum, id)
        }
    }
    return { totalIds, checksum }
}

export async function runOracleBenchmark({ samples = 20, fixtureNames = null } = {}) {
    if (!Number.isSafeInteger(samples) || samples <= 0 || samples > 1000) {
        throw new Error('samples must be a positive integer at most 1000')
    }

    const corpus = await loadTokenizerCorpus()
    const artifactHashes = await verifyArtifactHashes(corpus)
    const parity = await verifyTokenizerCorpus(corpus)
    const measurements = []

    for (const tokenizerId of TOKENIZER_IDS) {
        const tokenizer = createOracleEncoder(tokenizerId)
        try {
            const fixtures = createOracleBenchmarkFixtures(corpus, tokenizerId).filter(
                (fixture) => !fixtureNames || fixtureNames.has(fixture.name),
            )
            if (fixtures.length === 0) throw new Error('No benchmark fixture matched the requested names')

            for (const fixture of fixtures) {
                for (const mode of MODES) {
                    encodeBatch(tokenizer, fixture.texts, mode)
                    const durationsMs = []
                    let output = null
                    for (let sample = 0; sample < samples; sample++) {
                        const startedAt = performance.now()
                        output = encodeBatch(tokenizer, fixture.texts, mode)
                        durationsMs.push(performance.now() - startedAt)
                    }
                    measurements.push({
                        tokenizerId,
                        fixture: fixture.name,
                        mode,
                        samples,
                        segments: fixture.texts.length,
                        inputBytes: fixture.texts.reduce(
                            (total, text) => total + Buffer.byteLength(text, 'utf8'),
                            0,
                        ),
                        p50Ms: percentile(durationsMs, 0.5),
                        p95Ms: percentile(durationsMs, 0.95),
                        ...summarizeOutput(output, mode),
                    })
                }
            }
        } finally {
            tokenizer.free()
        }
    }

    return {
        schemaVersion: 1,
        scope: 'javascript-oracle-baseline',
        productionRoutingChanged: false,
        nativeCandidateMeasured: false,
        oracle: corpus.oracle,
        artifactHashes,
        parity,
        measurements,
    }
}

async function main() {
    const options = parseArguments(process.argv.slice(2))
    const result = await runOracleBenchmark(options)
    const serialized = `${JSON.stringify(result, null, 2)}\n`
    if (options.output) await writeFile(path.resolve(options.output), serialized)
    process.stdout.write(serialized)
}

const invokedPath = process.argv[1] ? path.resolve(process.argv[1]) : ''
if (invokedPath === path.resolve(fileURLToPath(import.meta.url))) {
    await main()
}
