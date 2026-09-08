interface CooperativeScheduler {
    yield(this: CooperativeScheduler): Promise<void>
}

const UI_YIELD_TIMEOUT_MS = 50

export function yieldToMainThread(): Promise<void> {
    const taskScheduler = (
        globalThis as typeof globalThis & {
            scheduler?: Partial<CooperativeScheduler>
        }
    ).scheduler
    if (typeof taskScheduler?.yield === 'function') {
        const schedulerWithYield = taskScheduler as CooperativeScheduler
        return schedulerWithYield.yield()
    }

    return new Promise((resolve) => setTimeout(resolve, 0))
}

export function yieldToUi(): Promise<void> {
    if (
        typeof document === 'undefined' ||
        document.visibilityState === 'hidden' ||
        typeof requestAnimationFrame !== 'function'
    ) {
        return yieldToMainThread()
    }

    return new Promise((resolve) => {
        let settled = false
        let frameId: number | undefined
        let frameTask: ReturnType<typeof setTimeout> | undefined

        const settle = () => {
            if (settled) return
            settled = true
            if (
                frameId !== undefined &&
                typeof cancelAnimationFrame === 'function'
            ) {
                cancelAnimationFrame(frameId)
            }
            if (frameTask !== undefined) clearTimeout(frameTask)
            clearTimeout(safetyTimer)
            resolve()
        }

        const safetyTimer = setTimeout(settle, UI_YIELD_TIMEOUT_MS)
        frameId = requestAnimationFrame(() => {
            if (settled) return
            frameTask = setTimeout(settle, 0)
        })
    })
}
