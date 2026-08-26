import { invoke } from '@tauri-apps/api/core'

import type { RegexExecutionResult } from './regexExecutionPlan'
import type { RegexSafePlan } from './regexSafePlan'

export interface NativeRegexBatchRuleError {
    sourceIndex: number
    category: string
}

interface NativeRegexBatchResult {
    data: string
    errors: NativeRegexBatchRuleError[]
}

export interface NativeRegexBatchOptions {
    signal?: AbortSignal
}

export interface NativeRegexBatchDependencies {
    invoke(
        command: string,
        args: Record<string, unknown>,
    ): Promise<unknown>
}

const productionDependencies: NativeRegexBatchDependencies = {
    invoke: (command, args) => invoke(command, args),
}

export class NativeRegexBatchRejectedError extends Error {
    constructor(readonly errors: NativeRegexBatchRuleError[]) {
        super('Native regex batch returned rule errors')
        this.name = 'NativeRegexBatchRejectedError'
    }
}

function abortReason(signal: AbortSignal): unknown {
    return signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
}

function assertResult(value: unknown): NativeRegexBatchResult {
    if (
        typeof value !== 'object'
        || value === null
        || typeof (value as NativeRegexBatchResult).data !== 'string'
        || !Array.isArray((value as NativeRegexBatchResult).errors)
    ) {
        throw new Error('Native regex batch returned an invalid result')
    }
    return value as NativeRegexBatchResult
}

export async function executeNativeRegexBatch(
    plan: RegexSafePlan,
    input: string,
    options: NativeRegexBatchOptions = {},
    dependencies: NativeRegexBatchDependencies = productionDependencies,
): Promise<RegexExecutionResult> {
    if (options.signal?.aborted) {
        throw abortReason(options.signal)
    }

    const invocation = dependencies.invoke('regex_execute_batch', { plan, input })
    let abortListener: (() => void) | undefined
    const response = options.signal === undefined
        ? await invocation
        : await Promise.race([
            invocation,
            new Promise<never>((_resolve, reject) => {
                abortListener = () => reject(abortReason(options.signal!))
                options.signal!.addEventListener('abort', abortListener, { once: true })
                if (options.signal!.aborted) {
                    abortListener()
                }
            }),
        ]).finally(() => {
            if (abortListener !== undefined) {
                options.signal!.removeEventListener('abort', abortListener)
            }
        })

    if (options.signal?.aborted) {
        throw abortReason(options.signal)
    }

    const result = assertResult(response)
    if (result.errors.length > 0) {
        throw new NativeRegexBatchRejectedError(result.errors)
    }
    return { data: result.data, errors: [] }
}
