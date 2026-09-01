import type { PeerSyncForegroundBridge, PeerSyncInvoke } from './peerSyncShared'

export interface PeerAndroidSourceForegroundIdentity<Lane extends string = string> {
    lane: Lane
    operationId: string
    generation: number
}

export interface PeerAndroidSourceForegroundOptions<
    Identity extends PeerAndroidSourceForegroundIdentity,
    Result,
> {
    invoke: PeerSyncInvoke
    bridge: PeerSyncForegroundBridge
    lane: Identity['lane']
    reserveCommand: string
    startCommand: string
    startArgs(sessionId: string, foreground: Identity): Record<string, unknown>
    staleIdentityError: string
    serviceStartError: string
    serviceStopError: string
    cleanupFailureMessage: string
}

export function createPeerAndroidSourceForeground<
    Identity extends PeerAndroidSourceForegroundIdentity,
    Result,
>(options: PeerAndroidSourceForegroundOptions<Identity, Result>) {
    const abandon = async (foreground: Identity): Promise<void> => {
        const abandoned = await options.invoke<boolean>('peer_sync_foreground_source_abandon', { foreground })
        if (!abandoned) throw new Error(options.staleIdentityError)
    }
    const stopAndAbandon = async (foreground: Identity): Promise<void> => {
        if (!options.bridge.stopSource(foreground.lane, foreground.operationId, foreground.generation)) {
            throw new Error(options.serviceStopError)
        }
        await abandon(foreground)
    }
    const failAfterCleanup = async <T>(primary: unknown, cleanup: () => Promise<void>): Promise<T> => {
        try {
            await cleanup()
        } catch (cleanupError) {
            throw new AggregateError([primary, cleanupError], options.cleanupFailureMessage)
        }
        throw primary
    }
    const recover = async (): Promise<void> => {
        const pending = await options.invoke<Identity | null>('peer_sync_foreground_source_status', {
            lane: options.lane,
        })
        if (pending?.lane === options.lane) await stopAndAbandon(pending)
    }

    return {
        recover,
        async start(sessionId: string): Promise<{ foreground: Identity, result: Result }> {
            await recover()
            const foreground = await options.invoke<Identity>(options.reserveCommand)
            let started: boolean
            try {
                started = options.bridge.startSource(
                    foreground.lane,
                    foreground.operationId,
                    foreground.generation,
                )
            } catch (primary) {
                return failAfterCleanup(primary, () => stopAndAbandon(foreground))
            }
            if (!started) {
                return failAfterCleanup(new Error(options.serviceStartError), () => abandon(foreground))
            }
            try {
                const result = await options.invoke<Result>(
                    options.startCommand,
                    options.startArgs(sessionId, foreground),
                )
                return { foreground, result }
            } catch (primary) {
                return failAfterCleanup(primary, () => stopAndAbandon(foreground))
            }
        },
    }
}
