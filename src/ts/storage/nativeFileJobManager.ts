import { Mutex } from '../mutex'
import { writable } from 'svelte/store'

import {
    NativeFileJobActivationCommittedError,
    NativeFileJobError,
    resolveNativeFileJobStage,
    type NativeFileJobResult,
    type NativeFileJobStage,
    type NativeFileJobStatus,
    type NativeFileOperationFormat,
} from './nativeFileJobs'

export type { NativeFileOperationFormat } from './nativeFileJobs'

export type NativeFileOperationKind = 'import' | 'export'

/**
 * How the renderer presents the operation. `inline` keeps the settings-page
 * status row; `dialog` opens the shared progress dialog and publishes a
 * terminal outcome for it to show until the user dismisses it.
 */
export type NativeFileOperationPresentation = 'inline' | 'dialog'

export interface NativeFileOperationSource {
    name: string
    bytes?: number
}

export interface NativeFileOperationState {
    kind: NativeFileOperationKind
    presentation: NativeFileOperationPresentation
    format?: NativeFileOperationFormat
    startedAt: number
    source?: NativeFileOperationSource
    status?: NativeFileJobStatus
    /** Every distinct stage seen so far, in order, including phase-derived ones. */
    observedStages: NativeFileJobStage[]
    blocking: boolean
    cancelRequested: boolean
    partialWritesPossible: boolean
}

export type NativeFileOperationOutcomeState = 'succeeded' | 'failed' | 'cancelled'

export interface NativeFileOperationError {
    code: string
    message: string
    recoveryRequired: boolean
}

export interface NativeFileOperationOutcome {
    kind: NativeFileOperationKind
    format?: NativeFileOperationFormat
    startedAt: number
    finishedAt: number
    state: NativeFileOperationOutcomeState
    source?: NativeFileOperationSource
    /** Last status observed before the operation settled, including `detail` and `result`. */
    status?: NativeFileJobStatus
    observedStages: NativeFileJobStage[]
    result?: NativeFileJobResult
    error?: NativeFileOperationError
    warningCodes: string[]
    partialWritesPossible: boolean
}

export interface SharedNativeFileOperationContext {
    signal: AbortSignal
    onStatus(status: NativeFileJobStatus): void
    setBlocking(blocking: boolean): void
    setSource(source: NativeFileOperationSource): void
    setPartialWritesPossible(value: boolean): void
}

export interface SharedNativeFileOperationOptions {
    presentation?: NativeFileOperationPresentation
    format?: NativeFileOperationFormat
}

export const nativeFileOperation = writable<NativeFileOperationState | null>(null)
export const nativeFileOperationOutcome = writable<NativeFileOperationOutcome | null>(null)

/**
 * Which surface draws a dialog-presented operation. The onboarding takes
 * backup restores into its own panel while it is on screen; the shared
 * dialog draws everything else.
 */
export type NativeFileJobHost = 'dialog' | 'onboarding'
export const nativeFileJobHost = writable<NativeFileJobHost>('dialog')

let activeOperation: Promise<unknown> | null = null
let activeOperationKey: string | null = null
let activeController: AbortController | null = null
let activeState: NativeFileOperationState | null = null
const externalAndroidOperationMutex = new Mutex()

export class NativeFileOperationBusyError extends Error {
    constructor() {
        super('Another native file operation is already running')
        this.name = 'NativeFileOperationBusyError'
    }
}

type Settlement = { value: unknown } | { error: unknown }

function isAbortError(error: unknown): boolean {
    return error instanceof DOMException && error.name === 'AbortError'
}

function stringWarningCodes(value: unknown): string[] {
    if (!value || typeof value !== 'object' || !('warningCodes' in value)) return []
    const codes = (value as { warningCodes: unknown }).warningCodes
    if (!Array.isArray(codes)) return []
    return codes.filter((code): code is string => typeof code === 'string')
}

function mergeWarningCodes(...groups: string[][]): string[] {
    return [...new Set(groups.flat())]
}

function outcomeError(kind: NativeFileOperationKind, error: unknown): NativeFileOperationError {
    if (error instanceof NativeFileJobActivationCommittedError) {
        const cause = error.cause
        return {
            code: error.code,
            message: cause instanceof Error ? cause.message : String(cause),
            recoveryRequired: true,
        }
    }
    if (error instanceof NativeFileJobError) {
        return { code: error.code, message: error.message, recoveryRequired: false }
    }
    return {
        code: `${kind}-error`,
        message: error instanceof Error ? error.message : String(error),
        recoveryRequired: false,
    }
}

function outcomeFromSettlement(
    state: NativeFileOperationState,
    settlement: Settlement,
    finishedAt: number,
): NativeFileOperationOutcome | null {
    const statusWarnings = state.status?.result?.warningCodes ?? []
    const base = {
        kind: state.kind,
        ...(state.format ? { format: state.format } : {}),
        startedAt: state.startedAt,
        finishedAt,
        ...(state.source ? { source: state.source } : {}),
        ...(state.status ? { status: state.status } : {}),
        observedStages: state.observedStages,
        ...(state.status?.result ? { result: state.status.result } : {}),
        partialWritesPossible: state.partialWritesPossible,
    }
    if ('error' in settlement) {
        const warningCodes = mergeWarningCodes(statusWarnings, stringWarningCodes(settlement.error))
        if (isAbortError(settlement.error)) {
            return { ...base, state: 'cancelled', warningCodes }
        }
        return {
            ...base,
            state: 'failed',
            error: outcomeError(state.kind, settlement.error),
            warningCodes,
        }
    }
    // A null or undefined result means the user backed out of the picker
    // before any work started, so there is nothing to summarize.
    if (settlement.value === null || settlement.value === undefined) return null
    return {
        ...base,
        state: 'succeeded',
        warningCodes: mergeWarningCodes(stringWarningCodes(settlement.value), statusWarnings),
    }
}

function publishActiveState(): void {
    nativeFileOperation.set(activeState ? { ...activeState } : null)
}

function updateActiveState(patch: Partial<NativeFileOperationState>): void {
    if (!activeState) return
    activeState = { ...activeState, ...patch }
    publishActiveState()
}

function recordStatus(status: NativeFileJobStatus): void {
    if (!activeState) return
    const stage = resolveNativeFileJobStage(status, activeState.format)
    const observedStages = stage && activeState.observedStages.at(-1) !== stage
        ? [...activeState.observedStages, stage]
        : activeState.observedStages
    updateActiveState({ status, observedStages })
}

export function runSharedNativeFileOperation<T>(
    kind: NativeFileOperationKind,
    operationKey: string,
    operation: (context: SharedNativeFileOperationContext) => Promise<T>,
    options: SharedNativeFileOperationOptions = {},
): Promise<T> {
    if (activeOperation) {
        return activeOperationKey === operationKey
            ? activeOperation as Promise<T>
            : Promise.reject(new NativeFileOperationBusyError())
    }

    const controller = new AbortController()
    const presentation = options.presentation ?? 'inline'
    activeController = controller
    activeOperationKey = operationKey
    activeState = {
        kind,
        presentation,
        ...(options.format ? { format: options.format } : {}),
        startedAt: Date.now(),
        observedStages: [],
        blocking: false,
        cancelRequested: false,
        partialWritesPossible: false,
    }
    if (presentation === 'dialog') nativeFileOperationOutcome.set(null)
    publishActiveState()

    const settle = (settlement: Settlement) => {
        if (activeOperation !== promise) return
        const state = activeState
        activeOperation = null
        activeOperationKey = null
        activeController = null
        activeState = null
        if (state?.presentation === 'dialog') {
            const outcome = outcomeFromSettlement(state, settlement, Date.now())
            if (outcome) nativeFileOperationOutcome.set(outcome)
        }
        publishActiveState()
    }

    const promise: Promise<T> = operation({
        signal: controller.signal,
        onStatus: recordStatus,
        setBlocking: (blocking) => updateActiveState({ blocking }),
        setSource: (source) => updateActiveState({ source }),
        setPartialWritesPossible: (value) => updateActiveState({ partialWritesPossible: value }),
    }).then(
        (value) => {
            settle({ value })
            return value
        },
        (error: unknown) => {
            settle({ error })
            throw error
        },
    )
    activeOperation = promise
    return promise
}

export function runExternalAndroidNativeFileOperation<T>(
    kind: NativeFileOperationKind,
    operation: (context: SharedNativeFileOperationContext) => Promise<T>,
    options: SharedNativeFileOperationOptions = {},
): Promise<T> {
    return externalAndroidOperationMutex.runExclusive(async () => {
        while (activeOperation) {
            try {
                await activeOperation
            } catch {}
        }
        return await runSharedNativeFileOperation(
            kind,
            `external-android:${kind}`,
            operation,
            options,
        )
    })
}

export function cancelActiveNativeFileOperation(): void {
    if (!activeController) return
    activeController.abort()
    updateActiveState({ cancelRequested: true })
}

export function dismissNativeFileOperationOutcome(): void {
    nativeFileOperationOutcome.set(null)
}
