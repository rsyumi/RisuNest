import { get, writable, type Writable } from 'svelte/store'
import { isLibraryFileOperationReserved } from '../storage/libraryFileOperation'

let activeReservation: symbol | null = null
const doingChatState = writable(false)
export const doingChat: Writable<boolean> = {
    subscribe: doingChatState.subscribe,
    set(busy) {
        if (!busy && activeReservation) return
        doingChatState.set(busy)
    },
    update(updater) {
        doingChat.set(updater(get(doingChatState)))
    },
}

export interface GenerationReservation {
    isCurrent(): boolean
    release(options?: { preserveBusy?: boolean }): void
}

export function reserveGeneration(): GenerationReservation | null {
    if (activeReservation || get(doingChat) || isLibraryFileOperationReserved()) return null
    const token = Symbol('generation-reservation')
    activeReservation = token
    doingChat.set(true)
    let released = false
    return {
        isCurrent: () => !released && activeReservation === token,
        release(options) {
            if (released) return
            released = true
            if (activeReservation !== token) return
            activeReservation = null
            if (!options?.preserveBusy) doingChat.set(false)
        },
    }
}
