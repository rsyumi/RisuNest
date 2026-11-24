import { DBState } from '../stores.svelte'
import { commitPersistentUnitIntent, flushPendingDataLocally } from '../storage/persistentDataRuntime.svelte'
import { loadRisunestSettingDescriptions } from './risunestSettingDescriptions'
import type { RisunestSettingsDependencies } from './risunestSettingsAccess'

// The app side of the settings methods. Reads use the working copy the settings screens bind,
// which keeps the selected preset and persona as indexes where the store keeps their IDs.

export const productionRisunestSettingsDependencies: RisunestSettingsDependencies = {
    flushPendingData: (reason) => flushPendingDataLocally(reason),
    readRootValues(keys) {
        const database = DBState.db as unknown as Record<string, unknown>
        const values: Record<string, unknown> = {}
        for (const key of keys) {
            const value = database[key]
            if (value !== undefined) values[key] = $state.snapshot(value)
        }
        return values
    },
    commitRootValues: (reason, values) => commitPersistentUnitIntent(reason, values.map(([key, value]) => ({
        key: JSON.stringify(['root', key]),
        type: 'set' as const,
        value,
    }))),
    loadDescriptions: (keys) => loadRisunestSettingDescriptions(keys),
}
