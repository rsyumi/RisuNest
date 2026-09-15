import { writable } from 'svelte/store'
import type {
    SyncExitCoordinator,
    SyncExitDecision,
    SyncExitState,
} from './syncExitCoordinator'

export const syncExitDialogState = writable<SyncExitState>({ phase: 'idle' })

let coordinator: SyncExitCoordinator | undefined
let unsubscribe: (() => void) | undefined

export function configureSyncExitCoordinator(next: SyncExitCoordinator): void {
    unsubscribe?.()
    coordinator = next
    unsubscribe = coordinator.subscribe((state) => syncExitDialogState.set(state))
}

export function decideSyncExit(choice: SyncExitDecision): boolean {
    return coordinator?.decide(choice) ?? false
}

export interface CloseRequestedEventLike {
    preventDefault(): void
}

export interface CloseDrainWindow {
    onCloseRequested(
        handler: (event: CloseRequestedEventLike) => void | Promise<void>,
    ): Promise<() => void>
    destroy(): Promise<void>
}

export async function registerWindowCloseDrain(
    window: CloseDrainWindow,
    exitCoordinator: SyncExitCoordinator,
    reportError: (error: unknown) => void = (error) =>
        console.error('Window exit drain failed', error),
): Promise<() => void> {
    let closing = false
    let pending = false
    return window.onCloseRequested(async (event) => {
        if (closing) return
        event.preventDefault()
        if (pending) return
        pending = true
        try {
            if (await exitCoordinator.requestExit() !== 'exit') return
            closing = true
            await window.destroy()
        } catch (error) {
            closing = false
            reportError(error)
        } finally {
            if (!closing) pending = false
        }
    })
}
