import type { ImportIntakeDelivery } from './types'

// Contract placeholder; the intake worker replaces this module.

/** Starts the intake listeners; returns their disposer. */
export function installImportIntake(): () => void {
    return () => {}
}

/** Queues files that arrived together and handles them once the app is idle. */
export function submitImportDelivery(_delivery: ImportIntakeDelivery): Promise<void> {
    return Promise.reject(new Error('Import intake is not wired yet'))
}
