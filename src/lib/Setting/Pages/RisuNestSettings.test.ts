import { describe, expect, it } from 'vitest'

import settingsRawSource from '../Settings.svelte?raw'
import pageRawSource from './RisuNestSettings.svelte?raw'
import backupRestoreRawSource from './RisuNestBackupRestore.svelte?raw'
import tauriLibRawSource from '../../../../src-tauri/src/lib.rs?raw'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'

const normalizeNewlines = (source: string) => source.replace(/\r\n?/g, '\n')
const settingsSource = normalizeNewlines(settingsRawSource)
const pageSource = normalizeNewlines(pageRawSource)
const backupRestoreSource = normalizeNewlines(backupRestoreRawSource)
const tauriLibSource = normalizeNewlines(tauriLibRawSource)

function expectExactRustBlock(source: string, block: string): void {
    expect(source.split(block)).toHaveLength(2)
}

describe('RisuNest settings navigation', () => {
    it('places RisuNest and Tauri-only Device Sync before plugin-added entries', () => {
        expect(settingsSource).toContain('Wrench')
        expect(settingsSource).toContain('$SettingsMenuIndex === 17')
        expect(settingsSource).toContain('language.risuNest.menuTitle')
        expect(settingsSource).toContain('MonitorSmartphone')
        expect(settingsSource).toContain('{#if isTauri}')
        expect(settingsSource).toContain('$SettingsMenuIndex === 18')
        expect(settingsSource).toContain('{#each additionalSettingsMenu as menu}')
        expect(settingsSource.indexOf('$SettingsMenuIndex === 17')).toBeLessThan(settingsSource.indexOf('{#each additionalSettingsMenu as menu}'))
        expect(settingsSource.indexOf('$SettingsMenuIndex === 18')).toBeLessThan(settingsSource.indexOf('{#each additionalSettingsMenu as menu}'))
    })

    it('dispatches both dedicated pages', () => {
        expect(settingsSource).toContain('<RisuNestSettings />')
        expect(settingsSource).toContain('<DeviceSyncSettings />')
    })

    it('renders final RisuNest groups in order with platform gates', () => {
        const groups = [
            'RisuNestPerformanceSettings',
            'RisuNestSettingRows',
            'RisuNestStorageDashboard',
            'RisuNestBackupRestore',
            'RisuNestAndroidPlatform',
            'RisuNestLogViewer',
        ]
        const positions = groups.map((group) => pageSource.lastIndexOf(`<${group}`))

        expect(positions.every((position) => position >= 0)).toBe(true)
        expect(positions).toEqual([...positions].sort((left, right) => left - right))
        expect(pageSource).toContain('{#if isTauri}\n        <RisuNestStorageDashboard />')
        expect(pageSource).toContain('{#if isTauriAndroid}\n        <RisuNestAndroidPlatform />')
        expect(pageSource).toContain('{#if isTauri}\n        <RisuNestLogViewer />')
    })

    it('offers a section shortcut for every group on the page', () => {
        for (const id of ['risunest-perf', 'risunest-streaming', 'risunest-inlay', 'risunest-storage', 'risunest-backup', 'risunest-platform', 'risunest-diag']) {
            expect(pageSource).toContain(`'${id}'`)
        }
        expect(pageSource).toContain('scrollIntoView')
    })
})

describe('RisuNest backup and restore layout', () => {
    it('groups actions into file, restore, and official account rows', () => {
        for (const group of ['groupFiles', 'groupRestore', 'groupAccount']) {
            expect(backupRestoreSource).toContain(`{language.risuNest.backup.${group}}`)
            expect(languageEnglish.risuNest.backup[group as keyof typeof languageEnglish.risuNest.backup]).toEqual(expect.any(String))
            expect(languageKorean.risuNest.backup[group as keyof typeof languageKorean.risuNest.backup]).toEqual(expect.any(String))
        }
        expect(backupRestoreSource).toContain('data-backup-group')
        expect(backupRestoreSource).not.toContain('className="mt-2"')
        expect(backupRestoreSource.indexOf('{language.risuNest.backup.groupFiles}')).toBeLessThan(backupRestoreSource.indexOf('{language.risuNest.backup.groupRestore}'))
        expect(backupRestoreSource.indexOf('{language.risuNest.backup.groupRestore}')).toBeLessThan(backupRestoreSource.indexOf('{language.risuNest.backup.groupAccount}'))
    })

    it('keeps local snapshot restore above PocketRisu restore', () => {
        const localSnapshotRestore = backupRestoreSource.indexOf('{language.restoreLocalSnapshot}</Button>')
        const pocketRisuRestore = backupRestoreSource.indexOf('{language.loadPocketRisuBackup}</Button>')

        expect(localSnapshotRestore).toBeGreaterThanOrEqual(0)
        expect(pocketRisuRestore).toBeGreaterThanOrEqual(0)
        expect(localSnapshotRestore).toBeLessThan(pocketRisuRestore)
    })
})

describe('RisuNest native command integration', () => {
    const setupStart = tauriLibSource.indexOf('.setup(move |app| {')
    const setupEnd = tauriLibSource.indexOf('Ok(())', setupStart)
    const setupSource = tauriLibSource.slice(setupStart, setupEnd)
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
        'device_sync_source_reserve',
        'peer_clone_claim_registered_client',
        'peer_delta_pull_registered',
        'peer_delta_target_retained',
        'peer_delta_target_abandon',
        'peer_bidirectional_sync_registered',
        'peer_bidirectional_resolve_registered',
    ]

    it.each(commands)('registers %s exactly once', (command) => {
        expect(handlerSource.match(new RegExp(`\\b${command}\\b`, 'g')) ?? []).toHaveLength(1)
    })

    it('manages the target-specific unified sharing state exactly once', () => {
        expectExactRustBlock(setupSource, `#[cfg(desktop)]
            app.manage(peer_sync::shared_session::DeviceSyncSourceState::default());`)
        expectExactRustBlock(setupSource, `#[cfg(target_os = "android")]
            app.manage(peer_sync::shared_session::AndroidDeviceSyncSourceState::default());`)
    })

    it.each([
        'device_sync_prepare',
        'device_sync_start',
        'device_sync_status',
        'device_sync_stop',
        'device_sync_rotate_link',
    ])('exposes shared_session::%s on desktop and Android', (command) => {
        expectExactRustBlock(handlerSource, `#[cfg(any(desktop, target_os = "android"))]
            peer_sync::shared_session::${command},`)
    })

    it('registers the Android source reservation command only on Android', () => {
        expectExactRustBlock(handlerSource, `#[cfg(target_os = "android")]
            peer_sync::shared_session::device_sync_source_reserve,`)
    })

    it('uses target-exclusive canonical v2 clone claim handlers', () => {
        expectExactRustBlock(handlerSource, `#[cfg(desktop)]
            peer_sync::commands::peer_clone_claim_v2_client,`)
        expectExactRustBlock(handlerSource, `#[cfg(target_os = "android")]
            peer_sync::registered_target_commands::peer_clone_claim_v2_client,`)
    })

    it('exposes registered reconnect commands on both supported native targets', () => {
        for (const command of [
            'peer_clone_claim_registered_client',
            'peer_delta_pull_registered',
            'peer_bidirectional_sync_registered',
            'peer_bidirectional_resolve_registered',
        ]) {
            expectExactRustBlock(handlerSource, `#[cfg(any(desktop, target_os = "android"))]
            peer_sync::registered_target_commands::${command},`)
        }
    })

    it('exposes the retained delta completion commands on desktop and Android', () => {
        for (const command of ['peer_delta_target_retained', 'peer_delta_target_abandon']) {
            expectExactRustBlock(handlerSource, `#[cfg(any(desktop, target_os = "android"))]
            peer_sync::delta_commands::${command},`)
        }
    })

    it('preserves directional registry commands on desktop and Android', () => {
        for (const command of [
            'peer_sync_outgoing_devices',
            'peer_sync_incoming_sources',
            'peer_sync_remove_incoming_source',
            'peer_sync_revoke_outgoing_device',
        ]) {
            expectExactRustBlock(handlerSource, `#[cfg(any(desktop, target_os = "android"))]
            peer_sync::registry_commands::${command},`)
        }
    })
})

describe('RisuNest startup failure language schema', () => {
    const required = ['title', 'schemaUnsupported', 'storeOpen', 'unknown', 'restart', 'copyDetails', 'copied', 'dataPathWindows', 'dataPathAndroid', 'stage']

    it.each([languageEnglish, languageKorean])('contains every startup recovery string', (translation) => {
        for (const key of required) {
            expect(translation.risuNest.boot[key as keyof typeof translation.risuNest.boot])
                .toEqual(expect.any(String))
        }
    })

    it('names the Windows data folder the user has to clear', () => {
        for (const translation of [languageEnglish, languageKorean]) {
            expect(translation.risuNest.boot.dataPathWindows).toContain('%APPDATA%\\RisuNest\\')
        }
    })
})

describe('RisuNest sync language schema', () => {
    const required = ['share.title', 'share.stateOff', 'share.methodLan', 'share.fixedGuideBody', 'share.errorLanAddressUnavailable', 'errorGeneric', 'errorTransportUnavailable', 'devices.outgoingTitle', 'devices.revokeConfirm', 'work.cloneConfirm', 'work.linkInvalid', 'work.receiving', 'work.conflictSelectTarget', 'work.conflictBody', 'work.dismiss', 'work.deltaRetained', 'work.deltaRetainedResumable', 'work.deltaRetainedAmbiguous', 'work.deltaAbandonConfirm', 'work.unknownDevice', 'work.progressLabel', 'work.deltaConflictBothChanged', 'work.deltaConflictLocalChanged', 'work.bidirectionalSyncing', 'work.bidirectionalResumeRequired', 'work.bidirectionalSourceUnavailable', 'work.bidirectionalRefreshPending', 'work.bidirectionalEditsDiscarded', 'androidLanOnly', 'notificationsDisabledWarning']

    it.each([languageEnglish, languageKorean])('contains every required nested sync branch', (translation) => {
        for (const path of required) {
            const value = path.split('.').reduce<any>((current, key) => current?.[key], translation.risuNest.sync)
            expect(value).toEqual(expect.any(String))
        }
    })

    it.each([languageEnglish, languageKorean])('no longer carries the per-lane blocks', (translation) => {
        expect('peerClone' in translation).toBe(false)
        expect('peerDelta' in translation).toBe(false)
        expect('peerBidirectional' in translation).toBe(false)
    })
})
