import { beforeEach, describe, expect, it, vi } from 'vitest'

describe('app update settings', () => {
    beforeEach(() => {
        localStorage.clear()
        vi.resetModules()
    })

    it('uses defaults only when the dedicated key is absent', async () => {
        const settings = await import('./settings')
        expect(settings.getAppUpdateSettings()).toEqual({
            schema: 'risunest.app-update-settings/v1',
            autoUpdateCheck: true,
            skippedVersion: '',
            lastCheckedAt: 0,
        })
        expect(localStorage.getItem(settings.appUpdateSettingsKey)).toBeNull()
    })

    it('reports malformed settings without overwriting their raw value', async () => {
        localStorage.setItem('risuNestUpdateSettings', '{broken')
        const settings = await import('./settings')
        expect(() => settings.getAppUpdateSettings()).toThrow()
        expect(localStorage.getItem('risuNestUpdateSettings')).toBe('{broken')
    })

    it('writes only the update record and keeps general device settings unchanged', async () => {
        localStorage.setItem('risuNestDeviceSettings', 'general-settings')
        const settings = await import('./settings')
        settings.updateAppUpdateSettings({ autoUpdateCheck: false })
        expect(localStorage.getItem('risuNestDeviceSettings')).toBe('general-settings')
        expect(JSON.parse(localStorage.getItem(settings.appUpdateSettingsKey)!)).toMatchObject({ autoUpdateCheck: false })
    })
})
