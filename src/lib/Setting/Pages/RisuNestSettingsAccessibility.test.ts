import { describe, expect, it } from 'vitest'

import backupSource from './RisuNestBackupRestore.svelte?raw'
import performanceSource from './RisuNestPerformanceSettings.svelte?raw'
import storageSource from './RisuNestStorageDashboard.svelte?raw'
import englishSource from 'src/lang/en.ts?raw'
import androidSource from './RisuNestAndroidPlatform.svelte?raw'
import logSource from './RisuNestLogViewer.svelte?raw'
import { risuNestSettingsItems } from 'src/ts/setting/risuNestSettingsData'

describe('RisuNest settings accessibility and copy', () => {
    it('labels the performance profile as a pressed button group', () => {
        expect(performanceSource).toContain('role="group"')
        expect(performanceSource).toContain('aria-label={language.risuNest.perf.profile}')
        expect(performanceSource).toContain('aria-pressed=')
        expect(performanceSource).not.toContain('role="radio"')
        expect(performanceSource).not.toContain('text-white')
    })

    it('uses localized loading and async status regions', () => {
        expect(storageSource).toContain('{language.loading}')
        expect(storageSource).not.toContain('Loading...')
        expect(storageSource).toContain('aria-live="polite"')
        expect(backupSource).toContain('aria-live="polite"')
        expect(backupSource).not.toContain('console.error')
    })

    it('uses the theme error token', () => {
        expect(storageSource).not.toContain('text-red-500')
    })

    it('keeps the performance profile toggle at its content width', () => {
        expect(performanceSource).toMatch(/inline-flex[^"]*self-start|self-start[^"]*inline-flex/)
    })

    it('separates every section after the first with the same top margin', () => {
        for (const source of [storageSource, backupSource, androidSource, logSource]) {
            expect(source).toMatch(/<h2 class="[^"]*\bmt-6\b[^"]*"/)
            expect(source).not.toMatch(/<h2 class="[^"]*\bmt-2\b[^"]*"/)
        }
        expect(performanceSource).toMatch(/<h2 class="[^"]*\bmt-2\b[^"]*"/)
        const header = risuNestSettingsItems.find(({ id }) => id === 'risunest.inlay.header')
        expect(header?.classes).toContain('mt-6')
    })

    it('gives slider and number rows the same label spacing as select rows', () => {
        for (const id of ['risunest.inlay.quality', 'risunest.inlay.maxDimension']) {
            expect(risuNestSettingsItems.find((item) => item.id === id)?.classes).toContain('mt-4')
        }
    })

    it('styles the diagnostics toggle and buttons like the shared controls while keeping keyboard focus', () => {
        expect(logSource).toContain('class="sr-only"')
        expect(logSource).toContain('bg-darkbutton')
        expect(logSource).toContain('hover:bg-selected')
        expect(logSource).toContain('<svg')
        expect(logSource).not.toContain('✓')
    })

    it('uses the specified English link action', () => {
        expect(englishSource).toContain("newLink: 'Create new link'")
        expect(englishSource).not.toContain("newLink: 'Create a new link'")
    })
})
