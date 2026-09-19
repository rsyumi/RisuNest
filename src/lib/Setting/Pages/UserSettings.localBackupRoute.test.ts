import { describe, expect, it } from 'vitest'
import { parse } from 'svelte/compiler'

import { readFileSync } from 'node:fs'
const source = readFileSync('src/lib/Setting/Pages/UserSettings.svelte', 'utf8')
const backupSource = readFileSync('src/lib/Setting/Pages/RisuNestBackupRestore.svelte', 'utf8')
const storageSource = readFileSync('src/lib/Setting/Pages/RisuNestStorageDashboard.svelte', 'utf8')

describe('UserSettings local backup route', () => {
    it('makes the existing backup import action reachable on Android and uses the common native picker', () => {
        const backup = backupSource
        const ast = parse(backup)
        const guards: string[][] = []
        function visit(node: any, parents: string[] = []) {
            if (!node || typeof node !== 'object') return
            const conditions =
                node.type === 'IfBlock'
                    ? [
                          ...parents,
                          backup.slice(
                              node.expression.start,
                              node.expression.end,
                          ),
                      ]
                    : parents
            if (
                node.type === 'InlineComponent' &&
                node.name === 'SettingButton' &&
                /runRisuSaveOperation\(['"]import['"]\)/.test(
                    backup.slice(node.start, node.end),
                )
            ) {
                guards.push(conditions)
            }
            for (const value of Object.values(node)) {
                if (Array.isArray(value))
                    for (const child of value) visit(child, conditions)
                else if (value && typeof value === 'object')
                    visit(value, conditions)
            }
        }
        visit(ast.html)
        expect(guards).toHaveLength(1)
        for (const expression of guards[0]) {
            // Evaluate the actual Svelte template's platform guard for an Android build.
            expect(
                new Function(
                    'isTauri',
                    'isTauriDesktop',
                    'isTauriAndroid',
                    `return (${expression})`,
                )(true, false, true),
            ).toBe(true)
        }
        expect(backup).toContain(
            'if (isTauri) await restoreBackupFromSystemPicker()',
        )
        expect(backup).toContain(
            "if (isTauri) return runRisuSaveOperation('import')",
        )
    })
    it('routes native full backup and restore through the common production caller and retains the web adapter', () => {
        expect(source).toContain('exportPortableBackupFromSystemPicker')
        expect(source).toContain('restoreBackupFromSystemPicker')
        expect(source).toContain('exportRisuSaveFromSystemPicker')
        expect(source).toContain('language.portableBackup.dbOnly')
        expect(source).toContain('SaveLocalBackup()')
        expect(source).toContain('LoadLocalBackup()')
        expect(source).toMatch(
            /isTauri[\s\S]*?restoreBackupFromSystemPicker\(\)[\s\S]*?LoadLocalBackup\(\)/,
        )
        expect(source).toMatch(
            /await runLocalBackupOperation\(["']export["']\)/,
        )
        expect(source).toMatch(
            /await runLocalBackupOperation\(["']import["']\)/,
        )
    })

    it('keeps local backup and official account controls but removes legacy Drive controls', () => {
        expect(source).toContain('SavePartialLocalBackup()')
        expect(source).toContain('loadRisuAccountBackup')
        expect(source).not.toContain('checkDriver')
        expect(source).not.toContain('googleDriveConnection')
    })

    it('moves RisuNest backup and sync controls to the dedicated page', () => {
        expect(storageSource).toContain('restoreNativePersistentSnapshot')
        expect(source).not.toContain('restoreNativePersistentSnapshot')
        for (const control of [
            'runRisuSaveOperation',
            'openSyncConflictBackups()',
            'getNativeOfficialAccountFlow().publish',
            'getNativeOfficialAccountFlow().restore',
            'nativePublishController?.abort()',
        ]) {
            expect(backupSource).toContain(control)
            expect(source).not.toContain(control)
        }
    })

    it('keeps official account actions behind the existing account gate', () => {
        const restoreGroup = backupSource.indexOf(
            '{language.risuNest.backup.groupRestore}',
        )
        const accountGate = backupSource.indexOf(
            '{#if isTauri && DBState.db.account}',
        )
        const officialRestore = backupSource.indexOf(
            '{language.risuNest.backup.officialRestore}',
        )
        expect(restoreGroup).toBeGreaterThan(-1)
        expect(accountGate).toBeGreaterThan(restoreGroup)
        expect(officialRestore).toBeGreaterThan(accountGate)
    })

    it('uses localized safe copy for official backup actions and native failures', () => {

        for (const key of [
            'officialRestoreConfirm',
            'officialRestoreInlayWarning',
            'officialMissing',
            'officialPublishConfirm',
            'officialPublished',
            'actionFailed',
        ])
            expect(backupSource).toContain(`language.risuNest.backup.${key}`)

        expect(backupSource).not.toContain('status.phase}')
        expect(backupSource).not.toContain('${status.phase}')
        expect(backupSource).not.toContain(
            'alertError(error instanceof Error ? error : String(error))',
        )
    })
})
