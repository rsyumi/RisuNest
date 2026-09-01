export interface PeerSourcePollingOptions {
    intervalMilliseconds: number
    poll(): Promise<void>
}

export function createPeerSourcePolling(options: PeerSourcePollingOptions) {
    let timer: ReturnType<typeof setInterval> | undefined
    let inFlight = false

    const tick = async (): Promise<void> => {
        if (inFlight) return
        inFlight = true
        try {
            await options.poll()
        } finally {
            inFlight = false
        }
    }

    return {
        start(): void {
            if (timer) return
            timer = setInterval(() => void tick(), options.intervalMilliseconds)
        },
        stop(): void {
            if (timer) clearInterval(timer)
            timer = undefined
        },
    }
}
