import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import type {
    LifecycleExitCoordinator,
    LifecycleExitSyncPolicy,
} from './lifecycleCommit'

interface MacosExitBaseDependencies {
    respond(token: string, exit: boolean): Promise<void>
    reportError(error: unknown): void
}

interface MacosCoordinatedExitDependencies extends MacosExitBaseDependencies {
    coordinator: LifecycleExitCoordinator
}

interface MacosLegacyExitDependencies extends MacosExitBaseDependencies {
    flush(): Promise<void>
    checkpoint(): Promise<void>
    confirmExitWithoutSaving(): Promise<boolean>
    sync: LifecycleExitSyncPolicy
}

export type MacosExitDependencies =
    | MacosCoordinatedExitDependencies
    | MacosLegacyExitDependencies

/** One quit request settles the real save before any acknowledgement reaches Rust. */
export function createMacosExitHandler(dependencies: MacosExitDependencies) {
    let pending = false
    return async (token: string): Promise<void> => {
        if (pending) return
        pending = true
        let exit = false
        try {
            if ('coordinator' in dependencies) {
                exit = await dependencies.coordinator.requestExit() === 'exit'
                return
            }
            try {
                await dependencies.flush()
                await dependencies.checkpoint()
                exit = true
            } catch (error) {
                dependencies.reportError(error)
                exit = await dependencies.confirmExitWithoutSaving()
            }
            if (
                exit &&
                dependencies.sync.isSyncActive() &&
                dependencies.sync.hasPendingSync()
            ) {
                exit = await dependencies.sync.confirmExit()
            }
        } catch (error) {
            exit = false
            dependencies.reportError(error)
        } finally {
            try {
                await dependencies.respond(token, exit)
            } finally {
                pending = false
            }
        }
    }
}

export async function registerMacosLifecycle(
    dependencies:
        | Omit<MacosCoordinatedExitDependencies, 'respond' | 'reportError'>
        | Omit<MacosLegacyExitDependencies, 'respond' | 'reportError'>,
): Promise<() => void> {
    const handler = createMacosExitHandler({
        ...dependencies,
        respond: (token, exit) =>
            invoke('macos_exit_response', { token, exit }),
        reportError: (error) =>
            console.error('macOS quit settlement failed', error),
    })
    const unlisten = await listen<string>(
        'risu-macos-exit-requested',
        ({ payload }) => {
            void handler(payload).catch((error) =>
                console.error('macOS quit response failed', error),
            )
        },
    )
    try {
        await invoke('macos_lifecycle_ready')
    } catch (error) {
        unlisten()
        throw error
    }
    return unlisten
}
