import { describe, expect, it, vi } from 'vitest'

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('../platform', () => ({ isTauriAndroid: false, isTauriDesktop: false }))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: vi.fn(),
}))

import { exportNativeCharacterCardFromPicker } from './nativeCharacterCardExportRoute'

describe('native character card export route', () => {
    it('projects JSON metadata from the exact leased revision after destination selection', async () => {
        const calls: unknown[] = []
        const leasedCharacter = {
            name: 'Leased JSON',
            triggerscript: [{ comment: 'trigger' }],
            customscript: [{ comment: 'regex' }],
        }

        await exportNativeCharacterCardFromPicker(
            {
                characterId: 'json-character',
                suggestedName: 'Leased JSON.json',
                format: 'json-card',
                projectCharacter: (character) => {
                    const leased = character as typeof character & typeof leasedCharacter
                    return {
                        spec: 'chara_card_v3',
                        spec_version: '3.0',
                        data: {
                            name: leased.name,
                            extensions: {
                                risuai: {
                                    triggerscript: leased.triggerscript,
                                    customScripts: leased.customscript,
                                },
                            },
                        },
                    }
                },
            },
            {},
            {
                isDesktop: () => true,
                isAndroid: () => false,
                chooseDestination: async (suggestedName, format) => {
                    calls.push(['pick', suggestedName, format])
                    return 'C:\\chosen\\Leased JSON.json'
                },
                runtime: () => ({
                    revision: 42,
                    flushPendingData: async (reason) => { calls.push(['flush', reason]) },
                }),
                readCharacter: async (characterId, revision) => {
                    calls.push(['read', characterId, revision])
                    return leasedCharacter as never
                },
                runExport: async (input) => {
                    calls.push(['export', input])
                    return {
                        revision: 42,
                        sourceBytes: 512,
                        sourceSha256: 'a'.repeat(64),
                        characterCount: 1,
                        presetCount: 0,
                        warningCodes: [],
                    }
                },
            },
        )

        expect(calls).toEqual([
            ['pick', 'Leased JSON.json', 'json-card'],
            ['flush', 'native-character-card-export'],
            ['read', 'json-character', 42],
            ['export', {
                characterId: 'json-character',
                destination: { type: 'desktopPath', path: 'C:\\chosen\\Leased JSON.json' },
                expectedRevision: 42,
                format: 'json-card',
                metadata: {
                    spec: 'chara_card_v3',
                    spec_version: '3.0',
                    data: {
                        name: 'Leased JSON',
                        extensions: {
                            risuai: {
                                triggerscript: [{ comment: 'trigger' }],
                                customScripts: [{ comment: 'regex' }],
                            },
                        },
                    },
                },
            }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
        expect(JSON.stringify(calls)).not.toContain('data:image')
    })

    it('leaves Web and a cancelled picker on the compatibility path', async () => {
        const input = {
            characterId: 'json-character',
            suggestedName: 'JSON.json',
            format: 'json-card' as const,
            projectCharacter: () => ({ spec: 'chara_card_v3' }),
        }
        const base = {
            isAndroid: () => false,
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            readCharacter: async () => { throw new Error('must not read') },
            runExport: async () => { throw new Error('must not export') },
        }
        await expect(exportNativeCharacterCardFromPicker(input, {}, {
            ...base,
            isDesktop: () => false,
            chooseDestination: async () => 'unused',
        })).resolves.toBeUndefined()
        await expect(exportNativeCharacterCardFromPicker(input, {}, {
            ...base,
            isDesktop: () => true,
            chooseDestination: async () => null,
        })).resolves.toBeNull()
    })
})
