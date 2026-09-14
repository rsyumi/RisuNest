export async function renameBookmarkWithFeedback(
    rename: (requestName: (currentName: string) => Promise<string>) => Promise<boolean>,
    requestName: (currentName: string) => Promise<string>,
    reportFailure: () => void,
): Promise<boolean> {
    let cancelled = false
    try {
        const changed = await rename(async (currentName) => {
            const next = await requestName(currentName)
            cancelled = !next?.trim()
            return next
        })
        if (!changed && !cancelled) reportFailure()
        return changed
    } catch {
        reportFailure()
        return false
    }
}

export async function reportFailedBookmarkOperation(
    operation: () => Promise<boolean>,
    reportFailure: () => void,
): Promise<boolean> {
    try {
        const changed = await operation()
        if (!changed) reportFailure()
        return changed
    } catch {
        reportFailure()
        return false
    }
}
