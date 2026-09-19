import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'

const backupSource = readFileSync('src/lib/Setting/Pages/RisuNestBackupRestore.svelte', 'utf8')
const performanceSource = readFileSync('src/lib/Setting/Pages/RisuNestPerformanceSettings.svelte', 'utf8')
const storageSource = readFileSync('src/lib/Setting/Pages/RisuNestStorageDashboard.svelte', 'utf8')
const androidSource = readFileSync('src/lib/Setting/Pages/RisuNestAndroidPlatform.svelte', 'utf8')
const logSource = readFileSync('src/lib/Setting/Pages/RisuNestLogViewer.svelte', 'utf8')
const serverSyncSource = readFileSync('src/lib/Setting/ServerSync/ServerSyncConnection.svelte', 'utf8')
const segmentedSource = readFileSync('src/lib/Setting/RisuNest/SegmentedButtons.svelte', 'utf8')
const groupSource = readFileSync('src/lib/Setting/RisuNest/SettingGroup.svelte', 'utf8')
const rowSource = readFileSync('src/lib/Setting/RisuNest/SettingRow.svelte', 'utf8')

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
