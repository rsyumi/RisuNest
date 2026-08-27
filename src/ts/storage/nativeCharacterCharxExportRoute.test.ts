import { describe, expect, it, vi } from 'vitest'

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('../platform', () => ({ isTauriDesktop: false }))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: vi.fn(),
}))

import { exportNativeCharacterCharxFromPicker } from './nativeCharacterCharxExportRoute'

describe('native character CharX export route', () => {
    it('projects metadata from the exact leased revision after destination selection', async () => {
        const calls: unknown[] = []
        const capturedBeforePicker = { name: 'Before picker', desc: 'old description' }
        const leasedCharacter = { name: 'After picker', desc: 'new description' }
        let persistentCharacter = capturedBeforePicker

        const result = await exportNativeCharacterCharxFromPicker(
            {
                characterId: 'current-character',
                suggestedName: 'Current.charx',
                projectCharacter: (character) => {
                    calls.push(['project', character])
                    return {
                        card: { spec: 'chara_card_v3', data: { ...character } },
                        module: { name: `${character.name} Module`, id: 'module-id' },
                    }
                },
            },
            {},
            {
                isDesktop: () => true,
                chooseDestination: async (name) => {
                    calls.push(['pick', name])
                    persistentCharacter = leasedCharacter
                    return 'C:\\chosen\\Current.charx'
                },
                runtime: () => ({
                    revision: 9,
                    flushPendingData: async (reason) => { calls.push(['flush', reason]) },
                }),
                readCharacter: async (characterId, revision) => {
                    calls.push(['read', characterId, revision])
                    return persistentCharacter as never
                },
                runExport: async (input) => {
                    calls.push(['export', input])
                    return {
                        revision: 9,
                        sourceBytes: 1024,
                        sourceSha256: 'a'.repeat(64),
                        characterCount: 1,
                        presetCount: 0,
                        warningCodes: [],
                    }
                },
            },
        )

        expect(result?.characterCount).toBe(1)
        expect(calls).toEqual([
            ['pick', 'Current.charx'],
            ['flush', 'native-character-charx-export'],
            ['read', 'current-character', 9],
            ['project', leasedCharacter],
            ['export', {
                characterId: 'current-character',
                destination: 'C:\\chosen\\Current.charx',
                expectedRevision: 9,
                card: { spec: 'chara_card_v3', data: { ...leasedCharacter } },
                module: { name: 'After picker Module', id: 'module-id' },
            }],
        ])
    })

    it('leaves Web and a cancelled picker on the compatibility path', async () => {
        const runExport = async () => {
            throw new Error('native export must not run')
        }
        const input = {
            characterId: 'current-character',
            suggestedName: 'Current.charx',
            projectCharacter: () => ({
                card: { spec: 'chara_card_v3' },
                module: {},
            }),
        }

        await expect(exportNativeCharacterCharxFromPicker(input, {}, {
            isDesktop: () => false,
            chooseDestination: async () => 'unused',
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            readCharacter: async () => { throw new Error('must not read') },
            runExport,
        })).resolves.toBeUndefined()

        await expect(exportNativeCharacterCharxFromPicker(input, {}, {
            isDesktop: () => true,
            chooseDestination: async () => null,
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            readCharacter: async () => { throw new Error('must not read') },
            runExport,
        })).resolves.toBeNull()
    })
})
