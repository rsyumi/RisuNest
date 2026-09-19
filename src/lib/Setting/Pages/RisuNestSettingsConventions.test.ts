import { describe, expect, it } from 'vitest'

import backupSource from './RisuNestBackupRestore.svelte?raw'
import performanceSource from './RisuNestPerformanceSettings.svelte?raw'
import storageSource from './RisuNestStorageDashboard.svelte?raw'
import androidSource from './RisuNestAndroidPlatform.svelte?raw'
import logSource from './RisuNestLogViewer.svelte?raw'
import serverSyncSource from '../ServerSync/ServerSyncConnection.svelte?raw'
import segmentedSource from '../RisuNest/SegmentedButtons.svelte?raw'
import groupSource from '../RisuNest/SettingGroup.svelte?raw'
import rowSource from '../RisuNest/SettingRow.svelte?raw'

describe('RisuNest settings theme and layout conventions', () => {
    it('uses theme tokens rather than fixed foreground and error colors', () => {
        expect(segmentedSource).not.toContain('text-white')
        expect(storageSource).not.toContain('text-red-500')
    })

    it('keeps section containers under the shared setting group', () => {
        for (const source of [
            performanceSource, storageSource, backupSource,
            androidSource, logSource, serverSyncSource,
        ]) {
            expect(source).toContain('<SettingGroup')
        }
        expect(groupSource).toContain('@container')
    })

    it('uses container rather than viewport breakpoints for setting rows', () => {
        expect(rowSource).toMatch(/@(sm|md|lg|xl):/)
        expect(rowSource).not.toMatch(/\b(sm|md|lg):/)
    })

})
