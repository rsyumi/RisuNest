export function createBookmarkDisplayDatabase<T>(database: T) {
    let current = $state(database)
    return {
        get current() {
            return current
        },
        replace(next: T) {
            current = next
        },
    }
}
