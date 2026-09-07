export const PENDING_SAVE_BYTE_LIMIT = 1_048_576

/** A scheduling hint: stop once the immediate-flush threshold is reached. */
export function measurePendingDataSize(
    read: () => unknown,
    maximum = PENDING_SAVE_BYTE_LIMIT,
): number {
    const enough = Symbol('pending-save-size-limit')
    let minimumLength = 0
    let root = true
    try {
        const serialized = JSON.stringify(read(), function (key, value) {
            const omitted =
                value === undefined ||
                typeof value === 'function' ||
                typeof value === 'symbol'
            if (!root && !Array.isArray(this) && !omitted)
                minimumLength += key.length + 3
            root = false
            if (typeof value === 'string') minimumLength += value.length + 2
            else if (typeof value === 'object')
                minimumLength += value === null ? 4 : 2
            else if (typeof value === 'number' || typeof value === 'boolean')
                minimumLength += JSON.stringify(value).length
            else if (omitted && Array.isArray(this)) minimumLength += 4
            // This lower bound excludes escaping and separators. Throwing here
            // avoids allocating a JSON copy of a multi-megabyte string value.
            if (minimumLength >= maximum) throw enough
            return value
        })
        return Math.min(serialized?.length ?? 0, maximum)
    } catch (error) {
        return error === enough ? maximum : 0
    }
}
