import { describe, expect, it } from 'vitest'

import settingsSource from '../Settings.svelte?raw'
import pageSource from './RisuNestSettings.svelte?raw'
import deviceSyncSource from './DeviceSyncSettings.svelte?raw'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'

describe('RisuNest settings navigation', () => {
    it('places RisuNest and Tauri-only Device Sync before Support', () => {
        expect(settingsSource).toContain('Wrench')
        expect(settingsSource).toContain('$SettingsMenuIndex === 17')
        expect(settingsSource).toContain('language.risuNest.menuTitle')
        expect(settingsSource).toContain('MonitorSmartphone')
        expect(settingsSource).toContain('{#if isTauri}')
        expect(settingsSource).toContain('$SettingsMenuIndex === 18')
        expect(settingsSource.indexOf('$SettingsMenuIndex === 17')).toBeLessThan(settingsSource.indexOf('$SettingsMenuIndex === 77'))
        expect(settingsSource.indexOf('$SettingsMenuIndex === 18')).toBeLessThan(settingsSource.indexOf('$SettingsMenuIndex === 77'))
    })

    it('dispatches both dedicated pages and renders the initial controls', () => {
        expect(settingsSource).toContain('<RisuNestSettings />')
        expect(settingsSource).toContain('<DeviceSyncSettings />')
        expect(pageSource).toContain('RisuNestPerformanceSettings')
        expect(pageSource).toContain('SettingRenderer')
        expect(deviceSyncSource).toContain('PeerCloneAndroidSettings')
        expect(deviceSyncSource).toContain('PeerCloneSettings')
        expect(deviceSyncSource).toContain('isTauriAndroid')
    })
})

describe('RisuNest sync language schema', () => {
    const required = ['share.title', 'share.stateOff', 'share.methodLan', 'share.fixedGuideBody', 'devices.outgoingTitle', 'devices.revokeConfirm', 'work.cloneConfirm', 'work.conflictBody', 'work.dismiss', 'lanWarning', 'androidLanOnly']

    it.each([languageEnglish, languageKorean])('contains every required nested sync branch', (translation) => {
        for (const path of required) {
            const value = path.split('.').reduce<any>((current, key) => current?.[key], translation.risuNest.sync)
            expect(value).toEqual(expect.any(String))
        }
    })
})
