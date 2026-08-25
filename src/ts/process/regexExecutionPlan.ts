import type { customscript } from '../storage/database.svelte'
import { getRuntimePerformanceBudgets, subscribeRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import { ByteBudgetLru } from '../util/byteBudgetLru'

const metadataPattern = /<(.+?)>/g
const dataPattern = /{{data}}/g
const supportedFlags = /[^dgimsuvy]/g
const hostActions = new Set(['inject', 'move_top', 'move_bottom', 'repeat_back'])
const parserRiskPattern = /[<{]/

export interface RegexExecutionPlanEntry {
    sourceIndex: number
    script: customscript
    pattern: string
    replacement: string
    flags: string
    order: number
    actions: string[]
    dynamicPattern: boolean
    compiledRegex?: RegExp
    compileError?: unknown
    requiresHostExecution: boolean
}

export interface RegexExecutionPlan {
    revision: number
    mode: string
    entries: RegexExecutionPlanEntry[]
    workerEligible: boolean
    requiresHostExecution: boolean
}

export interface RegexExecutionError {
    sourceIndex: number
    error: unknown
}

export interface RegexExecutionResult {
    data: string
    errors: RegexExecutionError[]
}

function createPlanCache() {
    return new ByteBudgetLru<string, RegexExecutionPlan>(
        Number.POSITIVE_INFINITY,
        () => 0,
        getRuntimePerformanceBudgets().regexPlanCacheEntries,
    )
}

let planCache = createPlanCache()
subscribeRuntimePerformanceProfile(() => {
    planCache = createPlanCache()
})
let nextPlanRevision = 1

function makePlanKey(scripts: customscript[], mode: string): string {
    return JSON.stringify([
        mode,
        scripts
            .filter((script) => script.type === mode)
            .map((script) => [
                script.type,
                script.in,
                script.out,
                script.flag ?? '',
                script.ableFlag ? 1 : 0,
            ]),
    ])
}

function normalizeFlags(script: customscript, actions: string[], replacement: string): string {
    let flags = script.ableFlag ? script.flag || 'g' : 'g'
    if (
        replacement.startsWith('@@move_top')
        || replacement.startsWith('@@move_bottom')
        || actions.includes('move_top')
        || actions.includes('move_bottom')
    ) {
        flags = flags.replace('g', '')
    }

    flags = flags.trim().replace(supportedFlags, '')
    flags = flags.split('').filter((value, index, values) => values.indexOf(value) === index).join('')
    return flags || 'u'
}

function parseEntry(script: customscript, sourceIndex: number): RegexExecutionPlanEntry {
    const parsedScript = { ...script }
    let order = 0
    const actions: string[] = []

    if (parsedScript.ableFlag && parsedScript.flag?.includes('<')) {
        parsedScript.flag = parsedScript.flag.replace(metadataPattern, (_value, metadata: string) => {
            for (const item of metadata.split(',').map((value) => value.trim())) {
                if (item.startsWith('order ')) {
                    order = parseInt(item.substring(6))
                }
                else {
                    actions.push(item)
                }
            }
            return ''
        })
    }

    let replacement = parsedScript.out.replaceAll('$n', '\n').replace(dataPattern, '$&')
    if (replacement.endsWith('>') && !actions.includes('no_end_nl')) {
        replacement += '\n'
    }

    const flags = normalizeFlags(parsedScript, actions, replacement)
    const dynamicPattern = actions.includes('cbs')
    const directive = replacement.startsWith('@@')
    const requiresHostExecution = directive || actions.some((action) => hostActions.has(action))
    const entry: RegexExecutionPlanEntry = {
        sourceIndex,
        script: parsedScript,
        pattern: parsedScript.in,
        replacement,
        flags,
        order,
        actions,
        dynamicPattern,
        requiresHostExecution,
    }

    if (!dynamicPattern && parsedScript.in !== '') {
        try {
            entry.compiledRegex = new RegExp(parsedScript.in, flags)
        }
        catch (error) {
            entry.compileError = error
        }
    }

    return entry
}

export function getRegexExecutionPlan(scripts: customscript[], mode: string): RegexExecutionPlan {
    const key = makePlanKey(scripts, mode)
    const cached = planCache.get(key)
    if (cached !== undefined) {
        return cached
    }

    const entries = scripts
        .map((script, sourceIndex) => ({ script, sourceIndex }))
        .filter(({ script }) => script.type === mode)
        .map(({ script, sourceIndex }) => parseEntry(script, sourceIndex))

    if (entries.some((entry) => entry.order !== 0)) {
        entries.sort((left, right) => right.order - left.order || left.sourceIndex - right.sourceIndex)
    }

    const requiresHostExecution = entries.some((entry) => entry.requiresHostExecution)
    const workerEligible = entries.every((entry) => (
        !entry.requiresHostExecution
        && entry.actions.length === 0
        && !entry.dynamicPattern
        && !parserRiskPattern.test(entry.replacement)
    ))
    const plan: RegexExecutionPlan = {
        revision: nextPlanRevision++,
        mode,
        entries,
        workerEligible,
        requiresHostExecution,
    }

    planCache.set(key, plan)
    return plan
}

export function canExecuteRegexPlanInWorker(plan: RegexExecutionPlan, input: string): boolean {
    return plan.workerEligible
        && plan.entries.some((entry) => entry.pattern !== '')
        && !parserRiskPattern.test(input)
}

function regexForEntry(entry: RegexExecutionPlanEntry, parse: (value: string) => string): RegExp {
    if (entry.dynamicPattern) {
        return new RegExp(parse(entry.pattern), entry.flags)
    }
    if (entry.compileError !== undefined) {
        throw entry.compileError
    }
    if (entry.compiledRegex === undefined) {
        throw new Error('Regex execution plan entry was not compiled')
    }
    return entry.compiledRegex
}

export function executeRegexPlanSync(
    plan: RegexExecutionPlan,
    input: string,
    parse: (value: string) => string,
): RegexExecutionResult {
    let data = input
    const errors: RegexExecutionError[] = []

    for (const entry of plan.entries) {
        if (entry.pattern === '') {
            continue
        }
        try {
            if (entry.requiresHostExecution) {
                throw new Error('Stateful regex action requires host execution')
            }

            const regex = regexForEntry(entry, parse)
            regex.lastIndex = 0
            if (entry.actions.length > 0) {
                if (regex.test(data)) {
                    regex.lastIndex = 0
                    data = parse(data.replace(regex, entry.replacement))
                }
            }
            else {
                data = parse(data.replace(regex, entry.replacement))
            }
        }
        catch (error) {
            errors.push({ sourceIndex: entry.sourceIndex, error })
            console.error(error)
        }
    }

    return { data, errors }
}
