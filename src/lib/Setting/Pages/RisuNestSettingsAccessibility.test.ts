import { describe, expect, it } from 'vitest'

import backupSource from './RisuNestBackupRestore.svelte?raw'
import performanceSource from './RisuNestPerformanceSettings.svelte?raw'
import storageSource from './RisuNestStorageDashboard.svelte?raw'
import englishSource from 'src/lang/en.ts?raw'

describe('RisuNest settings accessibility and copy', () => {
    it('labels the performance profile as a selected single-choice control', () => {
        expect(performanceSource).toContain('role="radiogroup"')
        expect(performanceSource).toContain('aria-label={language.risuNest.perf.profile}')
        expect(performanceSource).toContain('role="radio"')
        expect(performanceSource).toContain('aria-checked=')
    })

    it('uses localized loading and async status regions', () => {
        expect(storageSource).toContain('{language.loading}')
        expect(storageSource).not.toContain('Loading...')
        expect(storageSource).toContain('aria-live="polite"')
        expect(backupSource).toContain('aria-live="polite"')
    })

    it('uses the theme error token', () => {
        expect(storageSource).not.toContain('text-red-500')
    })

    it('uses the specified English link action', () => {
        expect(englishSource).toContain("newLink: 'Create new link'")
        expect(englishSource).not.toContain("newLink: 'Create a new link'")
    })
})
