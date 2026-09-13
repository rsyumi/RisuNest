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

describe('RisuNest settings navigation', () => {
    it('places RisuNest before plugin-added entries', () => {
        expect(settingsSource).toContain('Wrench')
        expect(settingsSource).toContain('$SettingsMenuIndex === 17')
        expect(settingsSource).toContain('language.risuNest.menuTitle')
        expect(settingsSource).toContain(
            '{#each additionalSettingsMenu as menu}',
        )
        expect(
            settingsSource.indexOf('$SettingsMenuIndex === 17'),
        ).toBeLessThan(
            settingsSource.indexOf('{#each additionalSettingsMenu as menu}'),
        )
    })

    it('dispatches the RisuNest page with server settings', () => {
        expect(settingsSource).toContain('<RisuNestSettings />')
        expect(pageSource).toContain('<ServerSyncSettings />')
        expect(settingsSource).not.toContain('DeviceSyncSettings')
        expect(settingsSource).not.toContain('$SettingsMenuIndex === 18')
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
        const positions = groups.map((group) =>
            pageSource.lastIndexOf(`<${group}`),
        )

        expect(positions.every((position) => position >= 0)).toBe(true)
        expect(positions).toEqual(
            [...positions].sort((left, right) => left - right),
        )
        expect(pageSource).toContain(
            '{#if isTauri}\n        <RisuNestStorageDashboard />',
        )
        expect(pageSource).toContain(
            '{#if isTauriAndroid}\n        <RisuNestAndroidPlatform />',
        )
        expect(pageSource).toContain(
            '{#if isTauri}\n        <RisuNestLogViewer />',
        )
    })

    it('offers a section shortcut for every group on the page', () => {
        for (const id of [
            'risunest-perf',
            'risunest-streaming',
            'risunest-inlay',
            'risunest-storage',
            'risunest-backup',
            'risunest-platform',
            'risunest-diag',
        ]) {
            expect(pageSource).toContain(`'${id}'`)
        }
        expect(pageSource).toContain('scrollIntoView')
    })
})

describe('RisuNest backup and restore layout', () => {
    it('groups actions into file, restore, and official account rows', () => {
        for (const group of ['groupFiles', 'groupRestore', 'groupAccount']) {
            expect(backupRestoreSource).toContain(
                `{language.risuNest.backup.${group}}`,
            )
            expect(
                languageEnglish.risuNest.backup[
                    group as keyof typeof languageEnglish.risuNest.backup
                ],
            ).toEqual(expect.any(String))
            expect(
                languageKorean.risuNest.backup[
                    group as keyof typeof languageKorean.risuNest.backup
                ],
            ).toEqual(expect.any(String))
        }
        expect(backupRestoreSource).toContain('data-backup-group')
        expect(backupRestoreSource).not.toContain('className="mt-2"')
        expect(
            backupRestoreSource.indexOf(
                '{language.risuNest.backup.groupFiles}',
            ),
        ).toBeLessThan(
            backupRestoreSource.indexOf(
                '{language.risuNest.backup.groupRestore}',
            ),
        )
        expect(
            backupRestoreSource.indexOf(
                '{language.risuNest.backup.groupRestore}',
            ),
        ).toBeLessThan(
            backupRestoreSource.indexOf(
                '{language.risuNest.backup.groupAccount}',
            ),
        )
    })

    it('keeps local snapshot restore above PocketRisu restore', () => {
        const localSnapshotRestore = backupRestoreSource.search(
            /\{language\.restoreLocalSnapshot\}<\/Button\s*>/,
        )
        const pocketRisuRestore = backupRestoreSource.search(
            /\{language\.loadPocketRisuBackup\}<\/Button\s*>/,
        )

        expect(localSnapshotRestore).toBeGreaterThanOrEqual(0)
        expect(pocketRisuRestore).toBeGreaterThanOrEqual(0)
        expect(localSnapshotRestore).toBeLessThan(pocketRisuRestore)
    })
})

describe('RisuNest native command integration', () => {
    const handlerStart = tauriLibSource.indexOf('pub fn invoke_handler()')
    const handlerEnd = tauriLibSource.indexOf(
        'pub fn handle_run_event(',
        handlerStart,
    )
    const handlerSource = tauriLibSource.slice(handlerStart, handlerEnd)
    it('connects the shared command router to the product builder', () => {
        expect(handlerStart).toBeGreaterThan(0)
        expect(handlerEnd).toBeGreaterThan(handlerStart)
        expect(handlerSource).toContain('tauri::generate_handler![')
        expect(tauriLibSource.slice(0, handlerStart)).toContain(
            '.invoke_handler(invoke_handler())',
        )
    })
    const commands = [
        'pds_storage_stats',
        'pds_snapshot_delete',
        'pds_asset_gc_preview',
        'pds_asset_gc_execute',
        'native_log_tail',
        'native_log_file_path',
        'native_log_set_file_enabled',
    ]

    it.each(commands)('registers %s exactly once', (command) => {
        expect(
            handlerSource.match(new RegExp(`\\b${command}\\b`, 'g')) ?? [],
        ).toHaveLength(1)
    })

    it('keeps server commands without a peer runtime', () => {
        expect(tauriLibSource).not.toContain('peer_sync')
        for (const command of [
            'server_sync_bind',
            'server_sync_backup_inventory',
            'server_sync_backup_delete',
            'server_sync_cache_usage',
            'server_sync_cache_cleanup',
        ]) {
            expect(
                handlerSource.match(new RegExp(`\\b${command}\\b`, 'g')) ?? [],
            ).toHaveLength(1)
        }
    })
})
describe('RisuNest startup failure language schema', () => {
    const required = [
        'title',
        'schemaUnsupported',
        'storeOpen',
        'unknown',
        'restart',
        'copyDetails',
        'copied',
        'dataPathWindows',
        'dataPathAndroid',
        'stage',
    ]

    it.each([languageEnglish, languageKorean])(
        'contains every startup recovery string',
        (translation) => {
            for (const key of required) {
                expect(
                    translation.risuNest.boot[
                        key as keyof typeof translation.risuNest.boot
                    ],
                ).toEqual(expect.any(String))
            }
        },
    )

    it('names the Windows data folder the user has to clear', () => {
        for (const translation of [languageEnglish, languageKorean]) {
            expect(translation.risuNest.boot.dataPathWindows).toContain(
                '%APPDATA%\\RisuNest\\',
            )
        }
    })
})
