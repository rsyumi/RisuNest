import { describe, expect, it, vi } from 'vitest'

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('../platform', () => ({ isTauriDesktop: false }))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: vi.fn(),
}))

import { exportNativeCharacterCharxFromPicker } from './nativeCharacterCharxExportRoute'

describe('native character CharX export route', () => {
    it('publishes a desktop CharX for the exact current character metadata', async () => {
        const calls: unknown[] = []
        const card = { spec: 'chara_card_v3', data: { name: 'Current' } }
        const module = { name: 'Current Module', id: 'module-id' }

        const result = await exportNativeCharacterCharxFromPicker(
            {
                characterId: 'current-character',
                suggestedName: 'Current.charx',
                card,
                module,
            },
            {},
            {
                isDesktop: () => true,
                chooseDestination: async (name) => {
                    calls.push(['pick', name])
                    return 'C:\\chosen\\Current.charx'
                },
                runtime: () => ({ revision: 9, flushPendingData: async () => undefined }),
                runExport: async (runtime, input) => {
                    calls.push(['export', runtime.revision, input])
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
            ['export', 9, {
                characterId: 'current-character',
                destination: 'C:\\chosen\\Current.charx',
                card,
                module,
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
            card: { spec: 'chara_card_v3' },
            module: {},
        }

        await expect(exportNativeCharacterCharxFromPicker(input, {}, {
            isDesktop: () => false,
            chooseDestination: async () => 'unused',
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            runExport,
        })).resolves.toBeUndefined()

        await expect(exportNativeCharacterCharxFromPicker(input, {}, {
            isDesktop: () => true,
            chooseDestination: async () => null,
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            runExport,
        })).resolves.toBeNull()
    })
})
