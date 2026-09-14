import { describe, expect, it } from 'vitest'

import backupSource from './RisuNestBackupRestore.svelte?raw'
import performanceSource from './RisuNestPerformanceSettings.svelte?raw'
import storageSource from './RisuNestStorageDashboard.svelte?raw'
import englishSource from 'src/lang/en.ts?raw'
import androidSource from './RisuNestAndroidPlatform.svelte?raw'
import logSource from './RisuNestLogViewer.svelte?raw'
import serverSyncSource from '../ServerSync/ServerSyncConnection.svelte?raw'
import segmentedSource from '../RisuNest/SegmentedButtons.svelte?raw'
import toggleSource from '../RisuNest/SettingToggle.svelte?raw'
import groupSource from '../RisuNest/SettingGroup.svelte?raw'
import rowSource from '../RisuNest/SettingRow.svelte?raw'
import { risuNestSettingUnits } from 'src/ts/setting/risuNestSettingsData'

describe('RisuNest settings accessibility and copy', () => {
    it('labels the performance profile as a pressed button group', () => {
        expect(performanceSource).toContain('<SegmentedButtons')
        expect(performanceSource).toContain('label={language.risuNest.perf.profile}')
        expect(segmentedSource).toContain('aria-pressed=')
        expect(segmentedSource).toContain("role === 'radiogroup' ? 'radio' : undefined")
        expect(segmentedSource).not.toContain('text-white')
    })

    it('uses localized loading and async status regions', () => {
        expect(storageSource).toContain('{language.loading}')
        expect(storageSource).not.toContain('Loading...')
        expect(storageSource).toContain('aria-live="polite"')
        expect(backupSource).not.toContain('console.error')
        expect(serverSyncSource).toContain('aria-live=')
    })

    it('uses the theme error token', () => {
        expect(storageSource).not.toContain('text-red-500')
    })

    it('renders every section through the shared group so headings and panels match', () => {
        for (const source of [
            performanceSource,
            storageSource,
            backupSource,
            androidSource,
            logSource,
            serverSyncSource,
        ]) {
            expect(source).toContain('<SettingGroup')
            expect(source).not.toMatch(/<h2\b/)
        }
        expect(groupSource).toContain('<h2 class="text-lg font-bold">{title}</h2>')
        expect(groupSource).toContain('@container')
    })

    it('stacks setting rows below the container breakpoint instead of the viewport', () => {
        expect(rowSource).toContain('grid-cols-1')
        expect(rowSource).toContain('@xl:grid-cols-[minmax(0,1fr)_auto]')
        expect(rowSource).not.toMatch(/\b(sm|md|lg):/)
    })

    it('shows the unit beside the maximum resolution input instead of inside its label', () => {
        expect(risuNestSettingUnits['risunest.inlay.maxDimension']).toBe('px')
        expect(englishSource).toContain("maxDimension: 'Maximum resolution'")
    })

    it('styles the shared toggle like the checkbox control while keeping keyboard focus', () => {
        expect(toggleSource).toContain('class="sr-only"')
        expect(toggleSource).toContain('bg-darkbutton')
        expect(toggleSource).toContain('focus-within:outline-darkborderc')
        expect(toggleSource).toContain('<svg')
        expect(toggleSource).not.toContain('✓')
        expect(logSource).toContain('<SettingToggle')
        expect(androidSource).toContain('<SettingToggle')
        expect(logSource).toContain('hover:bg-selected')
    })
})
