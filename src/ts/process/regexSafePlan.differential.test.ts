import { spawn } from 'node:child_process'
import { createHash, type Hash } from 'node:crypto'
import { once } from 'node:events'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'
import type { customscript } from '../storage/database.svelte'
import { getRegexExecutionPlan } from './regexExecutionPlan'
import { classifyRegexSafePlan } from './regexSafePlan'
import { createRegexWorkerMessageHandler } from './regexWorker'
import type { RegexWorkerResponse } from './regexWorkerClient'

const seed = 0x4b34_5afe
const defaultAllowedCases = 100_000
const defaultForbiddenCases = 10_000

function script(pattern: string, replacement: string, flag: string): customscript {
    return {
        comment: '',
        in: pattern,
        out: replacement,
        type: 'editoutput',
        flag,
        ableFlag: true,
    }
}

function createRandom(initialSeed: number): () => number {
    let state = initialSeed >>> 0
    return () => {
        state ^= state << 13
        state ^= state >>> 17
        state ^= state << 5
        return state >>> 0
    }
}

interface GeneratedPattern {
    pattern: string
    sample: string
    captures: number
}

function generatePattern(next: () => number): GeneratedPattern {
    const letters = 'abcdefghijklmnopqrstuvxyz'
    const a = letters[next() % letters.length]
    const b = letters[next() % letters.length]
    const c = letters[next() % letters.length]
    const min = 1 + next() % 2
    const max = min + next() % 3
    switch (next() % 8) {
        case 0:
            return { pattern: `${a}${b}?${c}`, sample: `${a}${b}${c}`, captures: 0 }
        case 1: {
            const rangeStart = next() % 20
            const rangeEnd = rangeStart + next() % 5
            const start = letters[rangeStart]
            const end = letters[rangeEnd]
            return {
                pattern: `[${start}-${end}]{${min},${max}}${c}`,
                sample: `${start.repeat(min)}${c}`,
                captures: 0,
            }
        }
        case 2:
            return {
                pattern: `(?:${a}|${b}){${min},${max}}${c}`,
                sample: `${a.repeat(min)}${c}`,
                captures: 0,
            }
        case 3:
            return { pattern: `(${a}|${b})[0-9]?`, sample: `${a}${next() % 10}`, captures: 1 }
        case 4:
            return { pattern: `(${a})|(${b}${c})`, sample: `${b}${c}`, captures: 2 }
        case 5: {
            const punctuation = ['.', '?', '+', '*', '(', ')', '[', ']', '|', '^', '$', '\\', '/']
            const literal = punctuation[next() % punctuation.length]
            return {
                pattern: `\\${literal}${a}{${min}}`,
                sample: `${literal}${a.repeat(min)}`,
                captures: 0,
            }
        }
        case 6:
            return {
                pattern: `[A-Z]?${a}(?:${b}|${c})`,
                sample: `A${a}${b}`,
                captures: 0,
            }
        default:
            return { pattern: `${a}{0,2}${b}`, sample: `${a}${b}`, captures: 0 }
    }
}

function generateReplacement(next: () => number, captures: number): string {
    const literal = String.fromCharCode(97 + next() % 26)
    switch (next() % 7) {
        case 0:
            return `value-${literal}`
        case 1:
            return '$$[$&]'
        case 2:
            return "[$`][$&][$']"
        case 3:
            return captures > 0 ? `$1-${literal}-$01` : `$99-${literal}`
        case 4:
            return captures > 1 ? '$2$1' : `$99${literal}`
        case 5:
            return `🙂$&${literal}`
        default:
            return `${literal}$&${literal}`
    }
}

function updateU32(hash: Hash, value: number): void {
    const bytes = Buffer.allocUnsafe(4)
    bytes.writeUInt32LE(value >>> 0)
    hash.update(bytes)
}

function updateValueHash(hash: Hash, id: number, value: string): void {
    const bytes = Buffer.from(value)
    updateU32(hash, id)
    updateU32(hash, bytes.byteLength)
    hash.update(bytes)
}

function updateResultHash(
    hash: Hash,
    id: number,
    data: string,
    errorSourceIndexes: number[],
): void {
    updateValueHash(hash, id, data)
    updateU32(hash, errorSourceIndexes.length)
    for (const sourceIndex of errorSourceIndexes) {
        updateU32(hash, sourceIndex)
    }
}

async function writeLine(
    child: ReturnType<typeof spawn>,
    value: object,
): Promise<void> {
    if (!child.stdin.write(`${JSON.stringify(value)}\n`)) {
        await once(child.stdin, 'drain')
    }
}

describe.skipIf(process.env.RISUNEST_REGEX_DIFFERENTIAL !== 'true')(
    'Rust regex JSONL differential profile',
    () => {
        it('keeps classifier nesting within the JSONL transport boundary', async () => {
            const boundaryDepth = 29
            const boundaryPattern = `${'(?:'.repeat(boundaryDepth)}a${')'.repeat(boundaryDepth)}`
            const concatPattern = `${'(?:'.repeat(boundaryDepth)}ab${')'.repeat(boundaryDepth)}`
            const classPattern = `${'(?:'.repeat(boundaryDepth)}[a-b]${')'.repeat(boundaryDepth)}`
            let structuredPattern = '[a-bx-z]'
            for (let depth = 0; depth < boundaryDepth; depth++) {
                structuredPattern = `(?:x${structuredPattern}|y)`
            }
            structuredPattern = `x${structuredPattern}|y`
            const overLimitPattern = `${'(?:'.repeat(boundaryDepth + 1)}a${')'.repeat(boundaryDepth + 1)}`
            const overLimit = classifyRegexSafePlan(
                getRegexExecutionPlan([script(overLimitPattern, 'x', 'g')], 'editoutput'),
                'a',
            )
            expect(overLimit).toEqual({
                accepted: false,
                category: 'regex_safe_nest_limit',
                sourceIndex: 0,
            })
            const boundaryCases = [
                { pattern: boundaryPattern, input: 'a' },
                { pattern: concatPattern, input: 'ab' },
                { pattern: classPattern, input: 'a' },
                {
                    pattern: structuredPattern,
                    input: `${'x'.repeat(boundaryDepth + 1)}a`,
                },
            ].map(({ pattern, input }, id) => {
                const executionPlan = getRegexExecutionPlan([
                    script(pattern, 'x', 'g'),
                ], 'editoutput')
                const classification = classifyRegexSafePlan(executionPlan, input)
                expect(classification).toMatchObject({ accepted: true })
                if (classification.accepted === false) {
                    throw new Error(`Boundary plan rejected: ${classification.category}`)
                }
                let response: RegexWorkerResponse | undefined
                const handleWorkerMessage = createRegexWorkerMessageHandler((value) => {
                    response = value
                })
                handleWorkerMessage({
                    type: 'register',
                    revision: executionPlan.revision,
                    entries: executionPlan.entries.map((entry) => [
                        entry.sourceIndex,
                        entry.pattern,
                        entry.replacement,
                        entry.flags,
                    ]),
                })
                handleWorkerMessage({
                    type: 'execute',
                    id,
                    revision: executionPlan.revision,
                    input,
                })
                if (response?.type !== 'result') {
                    throw new Error('JavaScript Worker did not return the boundary case')
                }
                return { id, classification, input, response }
            })

            const child = spawn(
                'cargo',
                [
                    'test',
                    '--locked',
                    '--lib',
                    'regex_shadow::tests::jsonl_differential_harness',
                    '--',
                    '--ignored',
                    '--exact',
                    '--nocapture',
                ],
                {
                    cwd: resolve(process.cwd(), 'src-tauri'),
                    env: {
                        ...process.env,
                    },
                    stdio: ['pipe', 'pipe', 'pipe'],
                },
            )
            let stdout = ''
            let stderr = ''
            child.stdout.setEncoding('utf8')
            child.stderr.setEncoding('utf8')
            child.stdout.on('data', (chunk: string) => {
                stdout += chunk
            })
            child.stderr.on('data', (chunk: string) => {
                stderr += chunk
            })
            const exit = once(child, 'exit')
            for (const boundaryCase of boundaryCases) {
                await writeLine(child, {
                    id: boundaryCase.id,
                    planJson: JSON.stringify(boundaryCase.classification.plan),
                    input: boundaryCase.input,
                    authorityData: boundaryCase.response.data,
                    authorityErrorSourceIndexes: boundaryCase.response.errors
                        .map(([sourceIndex]) => sourceIndex),
                })
            }
            child.stdin.end()
            const [exitCode] = await exit
            if (exitCode !== 0) {
                throw new Error(`Rust boundary harness failed with ${exitCode}: ${stderr.slice(-2_000)}`)
            }
            const marker = stdout
                .split(/\r?\n/)
                .find((line) => line.startsWith('RISUNEST_REGEX_DIFFERENTIAL '))
            if (marker === undefined) {
                throw new Error('Rust boundary harness did not emit a summary')
            }
            const summary = JSON.parse(
                marker.slice('RISUNEST_REGEX_DIFFERENTIAL '.length),
            ) as {
                allowedCases: number
                uniquePlans: number
                mismatches: number
                firstMismatch: unknown
            }

            expect(summary).toMatchObject({
                allowedCases: boundaryCases.length,
                uniquePlans: boundaryCases.length,
                mismatches: 0,
                firstMismatch: null,
            })
        }, 600_000)

        it('executes classifier-produced IR against JavaScript Worker authority', async () => {
            const allowedCases = Number(
                process.env.RISUNEST_REGEX_DIFFERENTIAL_CASES ?? defaultAllowedCases,
            )
            const forbiddenCases = Number(
                process.env.RISUNEST_REGEX_FORBIDDEN_CASES ?? defaultForbiddenCases,
            )
            const next = createRandom(seed)
            const child = spawn(
                'cargo',
                [
                    'test',
                    '--locked',
                    '--lib',
                    'regex_shadow::tests::jsonl_differential_harness',
                    '--',
                    '--ignored',
                    '--exact',
                    '--nocapture',
                ],
                {
                    cwd: resolve(process.cwd(), 'src-tauri'),
                    env: {
                        ...process.env,
                    },
                    stdio: ['pipe', 'pipe', 'pipe'],
                },
            )
            let stdout = ''
            let stderr = ''
            child.stdout.setEncoding('utf8')
            child.stderr.setEncoding('utf8')
            child.stdout.on('data', (chunk: string) => {
                stdout += chunk
            })
            child.stderr.on('data', (chunk: string) => {
                stderr += chunk
            })
            const exit = once(child, 'exit')
            let response: RegexWorkerResponse | undefined
            const planHash = createHash('sha256')
            const inputHash = createHash('sha256')
            const authorityHash = createHash('sha256')
            const uniquePlanJson = new Set<string>()
            const planPool = Array.from(
                { length: Math.min(512, allowedCases) },
                () => {
                    const entryCount = 1 + next() % 3
                    const generated = Array.from(
                        { length: entryCount },
                        () => generatePattern(next),
                    )
                    const scripts = generated.map(({ pattern, captures }) => (
                        script(
                            pattern,
                            generateReplacement(next, captures),
                            ['g', 'u', 'gu', 'ug'][next() % 4],
                        )
                    ))
                    const executionPlan = getRegexExecutionPlan(scripts, 'editoutput')
                    const seedInput = `🙂\r\n${
                        generated.map(({ sample }) => sample).join('|')
                    }|seed`
                    const classification = classifyRegexSafePlan(executionPlan, seedInput)
                    if (classification.accepted === false) {
                        throw new Error(`Allowed plan pool rejected: ${classification.category}`)
                    }
                    const handleWorkerMessage = createRegexWorkerMessageHandler((value) => {
                        response = value
                    })
                    handleWorkerMessage({
                        type: 'register',
                        revision: executionPlan.revision,
                        entries: executionPlan.entries.map((entry) => [
                            entry.sourceIndex,
                            entry.pattern,
                            entry.replacement,
                            entry.flags,
                        ]),
                    })
                    return { executionPlan, generated, handleWorkerMessage }
                },
            )

            for (let id = 0; id < allowedCases; id++) {
                const fixture = planPool[next() % planPool.length]
                const input = `🙂\r\n${
                    fixture.generated.map(({ sample }) => sample).join('|')
                }|case-${id}|${
                    String.fromCharCode(97 + next() % 26)
                }`
                const classification = classifyRegexSafePlan(fixture.executionPlan, input)
                if (classification.accepted === false) {
                    throw new Error(`Allowed generator rejected case ${id}: ${classification.category}`)
                }
                response = undefined
                fixture.handleWorkerMessage({
                    type: 'execute',
                    id,
                    revision: fixture.executionPlan.revision,
                    input,
                })
                if (response?.type !== 'result') {
                    throw new Error(`JavaScript Worker did not return case ${id}`)
                }
                const planJson = JSON.stringify(classification.plan)
                uniquePlanJson.add(planJson)
                const authorityErrorSourceIndexes = response.errors.map(([sourceIndex]) => sourceIndex)
                updateValueHash(planHash, id, planJson)
                updateValueHash(inputHash, id, input)
                updateResultHash(authorityHash, id, response.data, authorityErrorSourceIndexes)
                await writeLine(child, {
                    id,
                    planJson,
                    input,
                    authorityData: response.data,
                    authorityErrorSourceIndexes,
                })
            }
            child.stdin.end()
            const [exitCode] = await exit
            if (exitCode !== 0) {
                throw new Error(`Rust differential harness failed with ${exitCode}: ${stderr.slice(-2_000)}`)
            }
            const marker = stdout
                .split(/\r?\n/)
                .find((line) => line.startsWith('RISUNEST_REGEX_DIFFERENTIAL '))
            if (marker === undefined) {
                throw new Error('Rust differential harness did not emit a summary')
            }
            const rustSummary = JSON.parse(
                marker.slice('RISUNEST_REGEX_DIFFERENTIAL '.length),
            ) as {
                allowedCases: number
                uniquePlans: number
                mismatches: number
                planHash: string
                inputHash: string
                authorityHash: string
                rustHash: string
                firstMismatch: unknown
            }

            const forbiddenHash = createHash('sha256')
            let forbiddenAccepted = 0
            const mutations = [
                (pattern: string) => `(?=${pattern})${pattern}`,
                (pattern: string) => `(${pattern})\\1`,
                (pattern: string) => `^${pattern}`,
                (pattern: string) => `(?:${pattern})+`,
                (pattern: string) => `(?:${pattern})*`,
                (pattern: string) => `(?:${pattern})??x`,
                (pattern: string) => `(?<named>${pattern})`,
                (_pattern: string) => '[^a]',
                (_pattern: string) => '((a)|(b)){1,2}',
                (_pattern: string) => 'a{1,}',
            ]
            for (let id = 0; id < forbiddenCases; id++) {
                const base = generatePattern(next).pattern
                const pattern = mutations[id % mutations.length](base)
                const classification = classifyRegexSafePlan(
                    getRegexExecutionPlan([script(pattern, '$3', 'g')], 'editoutput'),
                    'ba',
                )
                if (classification.accepted) {
                    forbiddenAccepted++
                }
                const category = classification.accepted === false
                    ? classification.category
                    : 'accepted'
                updateValueHash(
                    forbiddenHash,
                    id,
                    `${pattern}\0${category}`,
                )
            }

            expect(rustSummary).toMatchObject({
                allowedCases,
                uniquePlans: uniquePlanJson.size,
                mismatches: 0,
                planHash: planHash.digest('hex'),
                inputHash: inputHash.digest('hex'),
                authorityHash: authorityHash.digest('hex'),
                firstMismatch: null,
            })
            expect(rustSummary.rustHash).toBe(rustSummary.authorityHash)
            expect(forbiddenAccepted).toBe(0)

            const evidence = {
                seed,
                allowedCases,
                uniquePlans: rustSummary.uniquePlans,
                forbiddenCases,
                forbiddenAccepted,
                planHash: rustSummary.planHash,
                inputHash: rustSummary.inputHash,
                authorityHash: rustSummary.authorityHash,
                rustHash: rustSummary.rustHash,
                forbiddenHash: forbiddenHash.digest('hex'),
            }
            const evidenceHash = createHash('sha256')
                .update(JSON.stringify(evidence))
                .digest('hex')
            console.log(`RISUNEST_REGEX_DIFFERENTIAL_EVIDENCE ${JSON.stringify({
                ...evidence,
                evidenceHash,
            })}`)
            if (allowedCases === defaultAllowedCases && forbiddenCases === defaultForbiddenCases) {
                expect(evidenceHash).toBe(
                    '410bcf34c8131ffa23782b867ea91d9c3386b47fc0c2c018f9f6b52bce0e1eed',
                )
            }
        }, 600_000)
    },
)
