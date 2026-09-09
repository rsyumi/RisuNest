import type { Database } from '../database.svelte'

export function createPersistentSaveObserverHarness(initial: Database) {
    let database = $state(initial)
    let selectedIndex = $state(0)
    let unrelated = $state(0)
    return {
        get database() {
            return database
        },
        set database(value: Database) {
            database = value
        },
        get selectedIndex() {
            return selectedIndex
        },
        set selectedIndex(value: number) {
            selectedIndex = value
        },
        get unrelated() {
            return unrelated
        },
        set unrelated(value: number) {
            unrelated = value
        },
    }
}
