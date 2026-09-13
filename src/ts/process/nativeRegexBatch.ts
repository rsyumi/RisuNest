import { invoke } from '@tauri-apps/api/core'
import { platform } from '@tauri-apps/plugin-os'

import type { RegexExecutionPlan, RegexExecutionResult } from './regexExecutionPlan'
import { classifyRegexSafePlan, type RegexSafePlan } from './regexSafePlan'

const NATIVE_REGEX_BATCH_RULES = 500
const NATIVE_REGEX_BATCH_MIN_INPUT_BYTES = 256 * 1024

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
    invoke(command: string, args: Record<string, unknown>): Promise<unknown>
    createRequestId?(): string
}

export interface NativeRegexBatchRouteDependencies extends NativeRegexBatchDependencies {
    isSupportedTauri(): boolean
}

const productionDependencies: NativeRegexBatchDependencies = {
    invoke: (command, args) => invoke(command, args),
    createRequestId: () => globalThis.crypto.randomUUID(),
}

export function isNativeRegexTauriRuntime(
    os?: string,
    tauriInternals = (
        globalThis as typeof globalThis & {
            __TAURI_INTERNALS__?: unknown
        }
    ).__TAURI_INTERNALS__,
): boolean {
    return Boolean(tauriInternals) && ['windows', 'android', 'linux'].includes(os ?? platform())
}

const productionRouteDependencies: NativeRegexBatchRouteDependencies = {
    ...productionDependencies,
    isSupportedTauri: () => isNativeRegexTauriRuntime(),
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
        typeof value !== 'object' ||
        value === null ||
        typeof (value as NativeRegexBatchResult).data !== 'string' ||
        !Array.isArray((value as NativeRegexBatchResult).errors)
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

    const requestId = dependencies.createRequestId?.() ?? globalThis.crypto.randomUUID()
    const invocation = dependencies.invoke('regex_execute_batch', {
        requestId,
        plan,
        input,
    })
    let abortListener: (() => void) | undefined
    const response =
        options.signal === undefined
            ? await invocation
            : await Promise.race([
                  invocation,
                  new Promise<never>((_resolve, reject) => {
                      abortListener = () => {
                          void dependencies
                              .invoke('regex_cancel_batch', { requestId })
                              .catch(() => {})
                          reject(abortReason(options.signal!))
                      }
                      options.signal!.addEventListener('abort', abortListener, {
                          once: true,
                      })
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

export async function tryExecuteNativeRegexBatch(
    executionPlan: RegexExecutionPlan,
    input: string,
    options: NativeRegexBatchOptions = {},
    dependencies: NativeRegexBatchRouteDependencies = productionRouteDependencies,
): Promise<RegexExecutionResult | undefined> {
    if (
        !dependencies.isSupportedTauri() ||
        executionPlan.entries.length !== NATIVE_REGEX_BATCH_RULES
    ) {
        return undefined
    }

    const classification = classifyRegexSafePlan(executionPlan, input, {
        minInputBytes: NATIVE_REGEX_BATCH_MIN_INPUT_BYTES,
    })
    if (classification.accepted === false) {
        return undefined
    }
    return executeNativeRegexBatch(classification.plan, input, options, dependencies)
}
