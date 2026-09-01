import { describe, expect, it } from 'vitest'

import settingsSource from '../Settings.svelte?raw'
import pageSource from './RisuNestSettings.svelte?raw'
import tauriLibSource from '../../../../src-tauri/src/lib.rs?raw'
import peerCloneAndroidSource from './PeerCloneAndroidSettings.svelte?raw'
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

    it('dispatches both dedicated pages', () => {
        expect(settingsSource).toContain('<RisuNestSettings />')
        expect(settingsSource).toContain('<DeviceSyncSettings />')
    })

    it('renders final RisuNest groups in order with platform gates', () => {
        const groups = [
            'RisuNestPerformanceSettings',
            'SettingRenderer',
            'RisuNestStorageDashboard',
            'RisuNestBackupRestore',
            'RisuNestAndroidPlatform',
            'RisuNestLogViewer',
        ]
        const positions = groups.map((group) => pageSource.lastIndexOf(`<${group}`))

        expect(positions.every((position) => position >= 0)).toBe(true)
        expect(positions).toEqual([...positions].sort((left, right) => left - right))
        expect(pageSource).toContain('{#if isTauri}\n    <RisuNestStorageDashboard />')
        expect(pageSource).toContain('{#if isTauriAndroid}\n    <RisuNestAndroidPlatform />')
        expect(pageSource).toContain('{#if isTauri}\n    <RisuNestLogViewer />')
    })
})

describe('RisuNest native command integration', () => {
    const handlerStart = tauriLibSource.indexOf('.invoke_handler(tauri::generate_handler![')
    const handlerEnd = tauriLibSource.indexOf('])\n        .build(', handlerStart)
    const handlerSource = tauriLibSource.slice(handlerStart, handlerEnd)
    const commands = [
        'pds_storage_stats',
        'pds_snapshot_delete',
        'pds_asset_gc_preview',
        'pds_asset_gc_execute',
        'peer_backup_list',
        'peer_backup_delete',
        'peer_temp_usage',
        'peer_temp_cleanup',
        'native_log_tail',
        'native_log_file_path',
        'native_log_set_file_enabled',
        'peer_sync_outgoing_devices',
        'peer_sync_incoming_sources',
        'peer_sync_remove_incoming_source',
        'peer_sync_revoke_outgoing_device',
        'device_sync_prepare',
        'device_sync_start',
        'device_sync_status',
        'device_sync_stop',
        'device_sync_rotate_link',
        'peer_sync_registered_hello',
        'peer_clone_claim_registered_client',
        'peer_delta_pull_registered',
        'peer_bidirectional_sync_registered',
        'peer_bidirectional_resolve_registered',
    ]

    it.each(commands)('registers %s exactly once', (command) => {
        expect(handlerSource.match(new RegExp(`\\b${command}\\b`, 'g')) ?? []).toHaveLength(1)
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

describe('temporary Android PeerClone Korean language coverage', () => {
    it('defines every peerClone key referenced by the component without English fallback', () => {
        const keys = [...peerCloneAndroidSource.matchAll(/language\.peerClone\.([A-Za-z0-9_]+)/g)].map((match) => match[1])
        for (const key of keys) {
            expect(languageKorean.peerClone[key as keyof typeof languageKorean.peerClone]).toEqual(expect.any(String))
        }
    })
})
