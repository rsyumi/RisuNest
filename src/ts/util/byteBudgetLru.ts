export class ByteBudgetLru<K, V> {
    private readonly entries = new Map<K, V>()
    private retainedBytes = 0

    constructor(
        private readonly maxBytes: number,
        private readonly measure: (key: K, value: V) => number,
        private readonly maxEntries = Number.POSITIVE_INFINITY,
    ) {}

    get(key: K): V | undefined {
        const value = this.entries.get(key)
        if (value === undefined) {
            return undefined
        }

        this.entries.delete(key)
        this.entries.set(key, value)
        return value
    }

    set(key: K, value: V): boolean {
        const bytes = this.measure(key, value)
        const existing = this.entries.get(key)
        if (existing !== undefined) {
            this.retainedBytes -= this.measure(key, existing)
            this.entries.delete(key)
        }

        if (this.maxBytes <= 0 || bytes > this.maxBytes) {
            return false
        }

        this.entries.set(key, value)
        this.retainedBytes += bytes
        this.trim()

        return this.entries.has(key)
    }

    private trim(): void {
        while (this.retainedBytes > this.maxBytes || this.entries.size > this.maxEntries) {
            const oldest = this.entries.entries().next().value
            if (!oldest) {
                break
            }
            const [oldestKey, oldestValue] = oldest
            this.entries.delete(oldestKey)
            this.retainedBytes -= this.measure(oldestKey, oldestValue)
        }
    }

    delete(key: K): boolean {
        const value = this.entries.get(key)
        if (value === undefined) {
            return false
        }
        this.entries.delete(key)
        this.retainedBytes -= this.measure(key, value)
        return true
    }

    clear(): void {
        this.entries.clear()
        this.retainedBytes = 0
    }

    get size(): number {
        return this.entries.size
    }

    get sizeBytes(): number {
        return this.retainedBytes
    }
}
