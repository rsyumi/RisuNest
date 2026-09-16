import { describe, expect, it } from 'vitest'

import {
    forgetPluginValueAssignment,
    pluginValuePreviewKey,
    recallPluginValueAssignment,
    rememberPluginValueAssignment,
} from './pluginValueAssignmentRetention'
import type {
    NativeStagedPluginChoice,
    NativeStagedPluginPreview,
} from './nativeFileJobs'

function preview(
    keys: readonly string[],
    pluginNames: readonly string[] = ['provider-manager'],
): NativeStagedPluginPreview {
    return {
        values: keys.map((key, index) => ({
            key,
            byteSize: 12 + index,
            valueType: 'json',
        })),
        pluginNames: [...pluginNames],
    }
}

const choice: NativeStagedPluginChoice = {
    assignments: [{ owner: 'provider-manager', keys: ['pm_store'] }],
    automatic: false,
}

describe('plugin value assignment retention', () => {
    it('reads the same key from a save staged twice in a different order', () => {
        const first = preview(['pm_store', 'pm_keys'])
        const second: NativeStagedPluginPreview = {
            values: [first.values[1], first.values[0]],
            pluginNames: ['provider-manager'],
        }

        expect(pluginValuePreviewKey(second)).toBe(pluginValuePreviewKey(first))
    })

    it('tells apart saves whose unowned values differ', () => {
        const base = preview(['pm_store', 'pm_keys'])
        const otherKeys = preview(['pm_store', 'yt_glossary'])
        const otherSize: NativeStagedPluginPreview = {
            values: [
                { ...base.values[0], byteSize: base.values[0].byteSize + 1 },
                base.values[1],
            ],
            pluginNames: [...base.pluginNames],
        }
        const otherPlugins = preview(['pm_store', 'pm_keys'], ['yumi-translator'])

        expect(pluginValuePreviewKey(otherKeys)).not.toBe(pluginValuePreviewKey(base))
        expect(pluginValuePreviewKey(otherSize)).not.toBe(pluginValuePreviewKey(base))
        expect(pluginValuePreviewKey(otherPlugins)).not.toBe(
            pluginValuePreviewKey(base),
        )
    })

    it('answers only the save the answers were given for', () => {
        const held = preview(['ra_one', 'ra_two'])
        const other = preview(['ra_one', 'ra_three'])
        rememberPluginValueAssignment(held, choice)

        expect(recallPluginValueAssignment(held)).toEqual(choice)
        expect(recallPluginValueAssignment(other)).toBeNull()

        forgetPluginValueAssignment(held)
        expect(recallPluginValueAssignment(held)).toBeNull()
    })

    it('hands back answers the caller cannot reach into', () => {
        const held = preview(['rb_one', 'rb_two'])
        rememberPluginValueAssignment(held, choice)

        const recalled = recallPluginValueAssignment(held)
        recalled?.assignments[0].keys.push('rb_two')

        expect(recallPluginValueAssignment(held)).toEqual(choice)
        forgetPluginValueAssignment(held)
    })

    it('keeps only the most recent saves', () => {
        const previews = ['c', 'd', 'e', 'f', 'g'].map((mark) =>
            preview([`r${mark}_one`, `r${mark}_two`]),
        )
        for (const held of previews) rememberPluginValueAssignment(held, choice)

        expect(recallPluginValueAssignment(previews[0])).toBeNull()
        expect(recallPluginValueAssignment(previews[4])).toEqual(choice)
        for (const held of previews) forgetPluginValueAssignment(held)
    })
})
