export async function reportPresetOperation(
    operation: () => Promise<unknown>,
    reportFailure: () => void,
): Promise<boolean> {
    try {
        await operation()
        return true
    } catch {
        reportFailure()
        return false
    }
}
