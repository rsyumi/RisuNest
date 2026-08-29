export async function resolveHypaOrphanState(
    query: () => Promise<boolean>,
): Promise<boolean> {
    try {
        return await query()
    } catch {
        return true
    }
}
