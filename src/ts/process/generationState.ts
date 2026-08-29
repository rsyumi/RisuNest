import { get, writable } from 'svelte/store'

export const doingChat = writable(false)

export interface GenerationReservation {
    isCurrent(): boolean
    release(): void
}

let activeReservation: symbol | null = null
doingChat.subscribe((busy) => {
    if (!busy) activeReservation = null
})

export function reserveGeneration(): GenerationReservation | null {
    if (activeReservation || get(doingChat)) return null
    const token = Symbol('generation-reservation')
    activeReservation = token
    doingChat.set(true)
    let released = false
    return {
        isCurrent: () => !released && activeReservation === token,
        release() {
            if (released) return
            released = true
            if (activeReservation !== token) return
            activeReservation = null
            doingChat.set(false)
        },
    }
}
