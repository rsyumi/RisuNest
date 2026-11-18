import { vi, type Mock } from 'vitest'
import type { MobileBackgroundTask, MobileBackgroundTaskAfterPicker, MobileTaskKind } from './mobileBackgroundTask'
import { measuredTaskPercent } from './taskPercent'

type Run = typeof import('./mobileBackgroundTask').runWithMobileBackgroundTask
type RunAfterPicker = typeof import('./mobileBackgroundTask').runWithMobileBackgroundTaskAfterPicker

/** A hold that protects nothing, as every hold is outside a mobile app. */
export function unprotectedMobileTask(signal?: AbortSignal): MobileBackgroundTask {
    return { signal, protection: { state: 'refused', reason: 'unavailable' }, progress() {}, async dispose() {} }
}

/**
 * The module as it behaves outside a mobile app: every hold is unprotected and work runs as it is.
 * Each member is a spy, so a test can observe it or replace its implementation.
 */
export function mobileBackgroundTaskModule() {
    const begin = (_kind: MobileTaskKind, signal?: AbortSignal): MobileBackgroundTask | Promise<MobileBackgroundTask> => unprotectedMobileTask(signal)
    async function run<T>(_kind: MobileTaskKind, operation: (task: MobileBackgroundTask) => Promise<T>, signal?: AbortSignal): Promise<T> {
        return operation(unprotectedMobileTask(signal))
    }
    async function runAfterPicker<T>(_kind: MobileTaskKind, operation: (task: MobileBackgroundTaskAfterPicker) => Promise<T>, signal?: AbortSignal): Promise<T> {
        return operation({ ...unprotectedMobileTask(signal), protect() {} })
    }
    return {
        hasMobileBackgroundTasks: vi.fn(() => false),
        subscribeMobileBackgroundTasks: vi.fn((_listener: () => void) => () => {}),
        retainMobileBackgroundActivity: vi.fn(() => () => {}),
        measuredTaskPercent: vi.fn(measuredTaskPercent),
        mobileDeferredHoldsSettled: vi.fn(async () => {}),
        beginMobileBackgroundTask: vi.fn(begin),
        beginDeferredMobileBackgroundTask: vi.fn(begin),
        // A spy loses the type parameter, so it is typed as the generic function it stands for.
        runWithMobileBackgroundTask: vi.fn(run) as Mock<Run> & Run,
        runWithDeferredMobileBackgroundTask: vi.fn(run) as Mock<Run> & Run,
        runWithMobileBackgroundTaskAfterPicker: vi.fn(runAfterPicker) as Mock<RunAfterPicker> & RunAfterPicker,
    } satisfies typeof import('./mobileBackgroundTask')
}
