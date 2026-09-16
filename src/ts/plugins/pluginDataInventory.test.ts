import { describe, expect, it, vi } from 'vitest'

vi.mock('../platform', () => ({ isTauri: false }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('./plugins.svelte', () => ({ pluginStorageStore: { invalidate: vi.fn() } }))
vi.mock('../storage/persistentDataStoreFactory', () => ({
    getPersistentDataStore: () => ({ listPluginStorage: async () => [] }),
}))

import {
    filterPluginDataItems,
    groupPluginDataByPrefix,
    pluginDataAssignmentPrefill,
    pluginDataItemId,
    pluginDataOwnerBuckets,
    pluginKeyPrefix,
    type PluginDataItem,
} from './pluginDataInventory'
import { UNOWNED_PLUGIN_OWNER } from './pluginOwner'

function item(
    owner: string,
    key: string,
    byteSize = 10,
    automatic = false,
): PluginDataItem {
    return { owner, key, valueType: 'json', byteSize, automatic }
}

describe('plugin data inventory', () => {
    it('reads a prefix only up to the first separator', () => {
        expect(pluginKeyPrefix('pm_store')).toBe('pm_')
        expect(pluginKeyPrefix('yumi-translator:glossary')).toBe('yumi-')
        expect(pluginKeyPrefix('legacyMemoryNote')).toBeNull()
        expect(pluginKeyPrefix('_leading')).toBeNull()
    })

    it('bundles only prefixes more than one key shares', () => {
        const groups = groupPluginDataByPrefix([
            item(UNOWNED_PLUGIN_OWNER, 'pm_store'),
            item(UNOWNED_PLUGIN_OWNER, 'pm_keys', 20),
            item(UNOWNED_PLUGIN_OWNER, 'yt_only'),
            item(UNOWNED_PLUGIN_OWNER, 'legacyMemoryNote'),
        ])
        expect(groups.map((group) => group.prefix)).toEqual(['pm_', null])
        expect(groups[0].byteSize).toBe(30)
        expect(groups[1].items.map((entry) => entry.key)).toEqual([
            'legacyMemoryNote',
            'yt_only',
        ])
    })

    it('counts each plugin and keeps the unknown bucket last', () => {
        const buckets = pluginDataOwnerBuckets([
            item('plugin-a', 'one', 5),
            item(UNOWNED_PLUGIN_OWNER, 'two', 50),
            item('plugin-b', 'three', 100),
            item('plugin-a', 'four', 5),
        ])
        expect(buckets.map((bucket) => bucket.owner)).toEqual([
            'plugin-b',
            'plugin-a',
            UNOWNED_PLUGIN_OWNER,
        ])
        expect(buckets[1]).toEqual({ owner: 'plugin-a', count: 2, byteSize: 10 })
    })

    it('narrows by plugin, key text, stored text and the automatic bucket', () => {
        const rows = [
            item('plugin-a', 'pm_store'),
            item('plugin-a', 'pm_keys'),
            item('plugin-b', 'pm_store'),
            item('plugin-a', 'auto_value', 10, true),
        ]
        const values = new Map([[pluginDataItemId(rows[0]), '{"apiKey":"secret"}']])

        expect(
            filterPluginDataItems(
                rows,
                { owner: 'plugin-a', automaticOnly: false, key: '', value: '' },
                values,
            ).map((row) => row.key),
        ).toEqual(['pm_store', 'pm_keys', 'auto_value'])
        expect(
            filterPluginDataItems(
                rows,
                { owner: null, automaticOnly: false, key: 'keys', value: '' },
                values,
            ).map((row) => row.key),
        ).toEqual(['pm_keys'])
        expect(
            filterPluginDataItems(
                rows,
                { owner: null, automaticOnly: false, key: '', value: 'apikey' },
                values,
            ).map((row) => row.owner),
        ).toEqual(['plugin-a'])
        expect(
            filterPluginDataItems(
                rows,
                { owner: null, automaticOnly: true, key: '', value: '' },
                values,
            ).map((row) => row.key),
        ).toEqual(['auto_value'])
    })

    it('opens on the assignments a cancelled import kept', () => {
        const items = [
            item(UNOWNED_PLUGIN_OWNER, 'pm_store'),
            item(UNOWNED_PLUGIN_OWNER, 'pm_keys'),
            item(UNOWNED_PLUGIN_OWNER, 'yt_glossary'),
        ]

        const prefill = pluginDataAssignmentPrefill(
            items,
            [
                { owner: 'provider-manager', keys: ['pm_store', 'pm_keys'] },
                { owner: 'yumi-translator', keys: ['yt_glossary'] },
            ],
            ['provider-manager', 'yumi-translator'],
        )

        expect(prefill.selectedIds).toEqual(items.map(pluginDataItemId))
        expect(prefill.groupOwners).toEqual([
            { prefix: 'pm_', owner: 'provider-manager' },
            { prefix: null, owner: 'yumi-translator' },
        ])
    })

    it('leaves out plugins that are not offered and keys the save no longer carries', () => {
        const items = [
            item(UNOWNED_PLUGIN_OWNER, 'pm_store'),
            item(UNOWNED_PLUGIN_OWNER, 'pm_keys'),
        ]

        const prefill = pluginDataAssignmentPrefill(
            items,
            [
                { owner: 'uninstalled-plugin', keys: ['pm_store'] },
                { owner: 'provider-manager', keys: ['pm_keys', 'pm_dropped'] },
            ],
            ['provider-manager'],
        )

        expect(prefill.selectedIds).toEqual([pluginDataItemId(items[1])])
        expect(prefill.groupOwners).toEqual([
            { prefix: 'pm_', owner: 'provider-manager' },
        ])
    })

    it('tells two plugins holding the same key apart', () => {
        expect(pluginDataItemId(item('plugin-a', 'shared'))).not.toBe(
            pluginDataItemId(item('plugin-b', 'shared')),
        )
    })
})
