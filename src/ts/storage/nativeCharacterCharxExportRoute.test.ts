import { describe, expect, it, vi } from 'vitest'

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('../platform', () => ({ isTauriDesktop: true, isTauriAndroid: false }))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: vi.fn(),
}))

import { save } from '@tauri-apps/plugin-dialog'

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
                isAndroid: () => false,
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
                destination: { type: 'desktopPath', path: 'C:\\chosen\\Current.charx' },
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
            isAndroid: () => false,
            chooseDestination: async () => 'unused',
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            readCharacter: async () => { throw new Error('must not read') },
            runExport,
        })).resolves.toBeUndefined()

        await expect(exportNativeCharacterCharxFromPicker(input, {}, {
            isDesktop: () => true,
            isAndroid: () => false,
            chooseDestination: async () => null,
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            readCharacter: async () => { throw new Error('must not read') },
            runExport,
        })).resolves.toBeNull()
    })

    it('routes Android directly to the SAF handoff without opening a desktop picker', async () => {
        const calls: unknown[] = []
        const leasedCharacter = { name: 'Android character', desc: 'leased' }

        await exportNativeCharacterCharxFromPicker(
            {
                characterId: 'android-character',
                suggestedName: 'Android character.charx',
                projectCharacter: (character) => ({
                    card: { spec: 'chara_card_v3', data: { ...character } },
                    module: {},
                }),
            },
            {},
            {
                isDesktop: () => false,
                isAndroid: () => true,
                chooseDestination: async () => {
                    throw new Error('desktop picker must not run')
                },
                runtime: () => ({
                    revision: 12,
                    flushPendingData: async (reason) => { calls.push(['flush', reason]) },
                }),
                readCharacter: async () => leasedCharacter as never,
                runExport: async (input) => {
                    calls.push(['export', input])
                    return {
                        revision: 12,
                        sourceBytes: 10,
                        sourceSha256: 'a'.repeat(64),
                        characterCount: 1,
                        presetCount: 0,
                        warningCodes: [],
                    }
                },
            },
        )

        expect(calls).toEqual([
            ['flush', 'native-character-charx-export'],
            ['export', {
                characterId: 'android-character',
                destination: { type: 'androidSaf', suggestedName: 'Android character.charx' },
                expectedRevision: 12,
                card: { spec: 'chara_card_v3', data: leasedCharacter },
                module: {},
            }],
        ])
    })

    it('matches the desktop dialog filter to the requested container', async () => {
        vi.mocked(save).mockResolvedValue(null)
        const base = {
            characterId: 'current-character',
            projectCharacter: () => ({ card: {}, module: {} }),
        }

        await expect(exportNativeCharacterCharxFromPicker({
            ...base,
            suggestedName: 'Current.jpeg',
            container: 'appended-charx-jpeg',
        })).resolves.toBeNull()
        expect(save).toHaveBeenLastCalledWith({
            defaultPath: 'Current.jpeg',
            filters: [{ name: 'CharX JPEG', extensions: ['jpeg'] }],
        })

        await expect(exportNativeCharacterCharxFromPicker({
            ...base,
            suggestedName: 'Current.charx',
        })).resolves.toBeNull()
        expect(save).toHaveBeenLastCalledWith({
            defaultPath: 'Current.charx',
            filters: [{ name: 'CharX', extensions: ['charx'] }],
        })
    })

    it('adds only the appended-JPEG discriminator while plain CharX remains omitted', async () => {
        const inputs: unknown[] = []
        const base = {
            characterId: 'current-character',
            suggestedName: 'Current.jpeg',
            projectCharacter: () => ({
                card: { spec: 'chara_card_v3' },
                module: {},
            }),
        }
        const dependencies = {
            isDesktop: () => false,
            isAndroid: () => true,
            chooseDestination: async () => null,
            runtime: () => ({ revision: 7, flushPendingData: async () => undefined }),
            readCharacter: async () => ({ name: 'Current' }) as never,
            runExport: async (input: unknown) => {
                inputs.push(input)
                return {
                    revision: 7,
                    sourceBytes: 1,
                    sourceSha256: 'a'.repeat(64),
                    characterCount: 1,
                    presetCount: 0,
                    warningCodes: [],
                }
            },
        }

        await exportNativeCharacterCharxFromPicker(base, {}, dependencies as never)
        await exportNativeCharacterCharxFromPicker({
            ...base,
            container: 'appended-charx-jpeg',
        }, {}, dependencies as never)

        expect(inputs).toEqual([
            expect.not.objectContaining({ container: expect.anything() }),
            expect.objectContaining({ container: 'appended-charx-jpeg' }),
        ])
    })
})
