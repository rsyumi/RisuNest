export interface PeerSourcePollingOptions {
    intervalMilliseconds: number
    poll(): Promise<void>
}

export function createPeerSourcePolling(options: PeerSourcePollingOptions) {
    let timer: ReturnType<typeof setInterval> | undefined
    let generation = 0
    let inFlightGeneration: number | undefined

    const tick = async (tickGeneration: number): Promise<void> => {
        if (tickGeneration !== generation || inFlightGeneration === tickGeneration) return
        inFlightGeneration = tickGeneration
        try {
            await options.poll()
        } finally {
            if (inFlightGeneration === tickGeneration) inFlightGeneration = undefined
        }
    }

    return {
        start(): void {
            if (timer) return
            const timerGeneration = ++generation
            timer = setInterval(() => void tick(timerGeneration), options.intervalMilliseconds)
        },
        stop(): void {
            generation += 1
            if (timer) clearInterval(timer)
            timer = undefined
        },
    }
}
