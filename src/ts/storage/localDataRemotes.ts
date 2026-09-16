import { getExternalStorageBridge } from './sync/external/bridge'
import { getServerSyncController } from './sync/serverSyncProduction'

/**
 * Whether anything that exchanges device sections is set up. `unknown` means
 * an answer has not arrived, so nothing may be claimed about it on screen.
 */
export type LocalDataRemoteState = 'unknown' | 'connected' | 'none'

function serverState(): LocalDataRemoteState {
    // Startup owns the first status read, so an absent one is simply not known
    // here rather than an answer of its own.
    const status = getServerSyncController().snapshot().status
    if (!status) return 'unknown'
    return status.configured ? 'connected' : 'none'
}

async function externalState(): Promise<LocalDataRemoteState> {
    const bridge = getExternalStorageBridge()
    if (!bridge.supported) return 'none'
    try {
        const state = await bridge.getState()
        return state.connections.length > 0 ? 'connected' : 'none'
    } catch {
        return 'unknown'
    }
}

export function combineLocalDataRemoteStates(
    states: LocalDataRemoteState[],
): LocalDataRemoteState {
    if (states.includes('connected')) return 'connected'
    if (states.includes('unknown')) return 'unknown'
    return 'none'
}

export async function readLocalDataRemoteState(): Promise<LocalDataRemoteState> {
    return combineLocalDataRemoteStates([serverState(), await externalState()])
}
