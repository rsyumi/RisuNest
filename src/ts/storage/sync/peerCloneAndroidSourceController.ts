import type { PeerCloneSourceStatus } from './peerClone'
import type { createAndroidPeerCloneSourceFacade } from './peerCloneAndroidSource'

export type AndroidPeerCloneSourceFacade = ReturnType<typeof createAndroidPeerCloneSourceFacade>

export function createAndroidPeerCloneSourceController(facade: AndroidPeerCloneSourceFacade) {
    let status: PeerCloneSourceStatus = { phase: 'idle', devices: [] }
    let busy = false

    async function run<T>(operation: () => Promise<T>): Promise<T> {
        if (busy) throw new Error('Android peer clone source operation is already active')
        busy = true
        try {
            return await operation()
        } finally {
            busy = false
        }
    }

    return {
        snapshot: () => ({ status, busy }),
        refresh: async () => (status = await facade.status()),
        prepare: () => run(async () => (status = await facade.prepare())),
        start: (sessionId: string) => run(async () => (status = await facade.start(sessionId))),
        stop: (sessionId: string) => run(async () => {
            await facade.stop(sessionId)
            status = await facade.status()
            return status
        }),
        revoke: (sessionId: string, deviceId: string) => run(async () => {
            await facade.revoke(sessionId, deviceId)
            status = await facade.status()
            return status
        }),
    }
}
