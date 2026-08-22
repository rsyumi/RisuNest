import { describe, expect, it } from 'vitest'
import {
    coldStorageHeader,
    getColdStorageAffectedCharacters,
    getColdStorageBackupKey,
    getColdStorageBackupName,
    isColdStorageBackupData,
    listCharacterResources,
    listColdDataKeysFromDb,
    listColdDataKeysFromCharacter,
    listDatabaseRootResources,
    replaceCharacterResources,
    replaceColdStoragePayloadResources,
    replaceDatabaseRootResources,
    decodeColdStoragePayload,
    encodeColdStoragePayload,
} from './coldstorageData'

describe('coldstorageData', () => {
    it.each([
        { character: { name: 'Current' } },
        { message: [{ data: 'Current chat' }] },
        [{ data: 'Legacy chat' }],
    ])('round trips current and legacy compressed JSON payloads', async (value) => {
        expect(await decodeColdStoragePayload(await encodeColdStoragePayload(value))).toEqual(value)
    })

    it('rejects unsupported values and invalid compressed JSON', async () => {
        await expect(encodeColdStoragePayload({ unrelated: true })).rejects.toThrow('unsupported')
        await expect(decodeColdStoragePayload(new Uint8Array([1, 2, 3]))).rejects.toThrow('compressed JSON')
    })

    it('lists and replaces the exact database root resource fields symmetrically', () => {
        const root = {
            customBackground: 'assets/background.png',
            userIcon: 'assets/user.png',
            modules: [{
                assets: [['module asset', 'assets/module.png', 'png']],
                icon: 'assets/module-icon.png',
            }],
            personas: [{
                icon: 'assets/persona.png',
                embeddedModule: {
                    assets: [['embedded asset', 'assets/embedded.png', 'png']],
                    icon: 'assets/embedded-icon.png',
                },
            }],
            characterOrder: [{
                name: 'Folder',
                imgFile: 'assets/folder.png',
            }],
        } as any
        const replacements = Object.fromEntries(
            listDatabaseRootResources(root).map((value) => [value, `remote/${value}`]),
        )

        const replaced = replaceDatabaseRootResources(root, replacements)

        expect(listDatabaseRootResources(root)).toEqual([
            'assets/background.png',
            'assets/user.png',
            'assets/module.png',
            'assets/module-icon.png',
            'assets/persona.png',
            'assets/embedded.png',
            'assets/embedded-icon.png',
            'assets/folder.png',
        ])
        expect(listDatabaseRootResources(replaced)).toEqual([
            'remote/assets/background.png',
            'remote/assets/user.png',
            'remote/assets/module.png',
            'remote/assets/module-icon.png',
            'remote/assets/persona.png',
            'remote/assets/embedded.png',
            'remote/assets/embedded-icon.png',
            'remote/assets/folder.png',
        ])
        expect(listDatabaseRootResources(root)[0]).toBe('assets/background.png')
    })

    it('lists and replaces the exact non-group character resource fields symmetrically', () => {
        const value = {
            type: 'character',
            image: 'assets/character.png',
            emotionImages: [['happy', 'assets/emotion.png']],
            additionalAssets: [['prop', 'assets/prop.png', 'png']],
            vits: { files: { model: 'assets/model.onnx', config: 'assets/config.json' } },
            ccAssets: [{
                type: 'icon',
                uri: 'assets/card.png',
                name: 'card',
                ext: 'png',
            }],
        } as any
        const replacements = Object.fromEntries(
            listCharacterResources(value).map((resource) => [resource, `remote/${resource}`]),
        )

        const replaced = replaceCharacterResources(value, replacements)

        expect(listCharacterResources(value)).toEqual([
            'assets/character.png',
            'assets/emotion.png',
            'assets/prop.png',
            'assets/model.onnx',
            'assets/config.json',
            'assets/card.png',
        ])
        expect(listCharacterResources(replaced)).toEqual([
            'remote/assets/character.png',
            'remote/assets/emotion.png',
            'remote/assets/prop.png',
            'remote/assets/model.onnx',
            'remote/assets/config.json',
            'remote/assets/card.png',
        ])
        expect(value.image).toBe('assets/character.png')
    })

    it('does not treat group-only compatibility fields as resources', () => {
        const group = {
            type: 'group',
            image: 'assets/group.png',
            emotionImages: [['happy', 'assets/group-emotion.png']],
            additionalAssets: [['prop', 'assets/ignored.png', 'png']],
            vits: { files: { model: 'assets/ignored.onnx' } },
        } as any

        expect(listCharacterResources(group)).toEqual([
            'assets/group.png',
            'assets/group-emotion.png',
        ])
    })

    it('replaces VITS and CC assets when additional assets are absent', () => {
        const value = {
            type: 'character',
            vits: { files: { model: 'assets/model.onnx' } },
            ccAssets: [{ uri: 'assets/card.png' }],
        } as any

        const replaced = replaceCharacterResources(value, {
            'assets/model.onnx': 'remote/model.onnx',
            'assets/card.png': 'remote/card.png',
        })

        expect(replaced.vits.files.model).toBe('remote/model.onnx')
        expect(replaced.ccAssets[0].uri).toBe('remote/card.png')
    })

    it('exports character cold keys directly for revision projections', () => {
        const value = {
            coldstorage: 'character-key',
            coldStoragedChats: ['stored-chat-key'],
            chats: [{ message: [{ data: coldStorageHeader + 'active-chat-key' }] }],
        } as any

        expect(listColdDataKeysFromCharacter(value)).toEqual([
            'character-key',
            'stored-chat-key',
            'active-chat-key',
        ])
    })

    it('lists cold-stored chat keys when the character itself is resident', () => {
        const value = {
            coldStoragedChats: ['stored-chat-key'],
            chats: [],
        } as any

        expect(listColdDataKeysFromCharacter(value)).toEqual(['stored-chat-key'])
    })

    it('collects unique character and chat cold storage keys from a database snapshot', () => {
        const characterKey = '11111111-1111-1111-1111-111111111111'
        const chatKey = '22222222-2222-2222-2222-222222222222'
        const nestedChatKey = '33333333-3333-3333-3333-333333333333'

        const keys = listColdDataKeysFromDb({
            characters: [
                {
                    coldstorage: characterKey,
                    coldStoragedChats: [chatKey, chatKey],
                    chats: [
                        {
                            message: [{
                                data: coldStorageHeader + nestedChatKey,
                            }],
                        },
                    ],
                },
                {
                    chats: [
                        {
                            message: [{
                                data: coldStorageHeader + chatKey,
                            }],
                        },
                    ],
                },
            ],
        } as any)

        expect(keys).toEqual([characterKey, chatKey, nestedChatKey])
    })

    it('maps unavailable cold storage keys to affected character names', () => {
        const characterKey = '11111111-1111-1111-1111-111111111111'
        const storedChatKey = '22222222-2222-2222-2222-222222222222'
        const activeChatKey = '33333333-3333-3333-3333-333333333333'
        const unknownKey = '44444444-4444-4444-4444-444444444444'

        const affected = getColdStorageAffectedCharacters({
            characters: [
                {
                    name: 'Stored Bot',
                    chaId: 'stored-bot',
                    coldstorage: characterKey,
                    coldStoragedChats: [storedChatKey],
                    chats: [],
                },
                {
                    name: 'Active Bot',
                    chaId: 'active-bot',
                    chats: [{
                        message: [{
                            data: coldStorageHeader + activeChatKey,
                        }],
                    }],
                },
                {
                    name: 'Healthy Bot',
                    chaId: 'healthy-bot',
                    chats: [],
                },
            ],
        } as any, [characterKey, storedChatKey, activeChatKey, unknownKey])

        expect(affected).toEqual({
            characterNames: ['Stored Bot', 'Active Bot'],
            unresolvedKeys: [unknownKey],
        })
    })

    it('uses the character id when an affected skeleton has no name', () => {
        const characterKey = '11111111-1111-1111-1111-111111111111'

        const affected = getColdStorageAffectedCharacters({
            characters: [{
                name: ' ',
                chaId: 'fallback-character-id',
                coldstorage: characterKey,
                chats: [],
            }],
        } as any, [characterKey])

        expect(affected.characterNames).toEqual(['fallback-character-id'])
        expect(affected.unresolvedKeys).toEqual([])
    })

    it('recognizes supported cold storage backup names', () => {
        const key = '11111111-1111-1111-1111-111111111111'

        expect(getColdStorageBackupName(key)).toBe(`coldstorage_${key}.json`)
        expect(getColdStorageBackupKey(`coldstorage_${key}.json`)).toBe(key)
        expect(getColdStorageBackupKey(`coldstorage/${key}.json`)).toBe(key)
        expect(getColdStorageBackupKey(`${key}.json`)).toBe(key)
        expect(getColdStorageBackupKey('assets/profile.png')).toBeNull()
    })

    it('accepts character, message, and legacy array cold storage payloads', () => {
        expect(isColdStorageBackupData({ character: {} })).toBe(true)
        expect(isColdStorageBackupData({ message: [] })).toBe(true)
        expect(isColdStorageBackupData([])).toBe(true)
        expect(isColdStorageBackupData({ nope: true })).toBe(false)
        expect(isColdStorageBackupData(null)).toBe(false)
    })

    it('rewrites character cold storage asset references without mutating the original payload', () => {
        const payload = {
            character: {
                type: 'character',
                image: 'assets/local-main.png',
                emotionImages: [
                    ['neutral', 'assets/local-neutral.png'],
                    ['happy', 'assets/local-happy.png'],
                ],
                additionalAssets: [
                    ['prop', 'assets/local-prop.png'],
                ],
                vits: { files: { model: 'assets/local-model.onnx' } },
                ccAssets: [{ uri: 'assets/local-card.png' }],
            },
        }

        const rewritten = replaceColdStoragePayloadResources(payload, {
            'assets/local-main.png': 'assets/account-main.png',
            'assets/local-neutral.png': 'assets/account-neutral.png',
            'assets/local-prop.png': 'assets/account-prop.png',
            'assets/local-model.onnx': 'assets/account-model.onnx',
            'assets/local-card.png': 'assets/account-card.png',
        }) as typeof payload

        expect(rewritten.character.image).toBe('assets/account-main.png')
        expect(rewritten.character.emotionImages[0][1]).toBe('assets/account-neutral.png')
        expect(rewritten.character.emotionImages[1][1]).toBe('assets/local-happy.png')
        expect(rewritten.character.additionalAssets[0][1]).toBe('assets/account-prop.png')
        expect(rewritten.character.vits.files.model).toBe('assets/account-model.onnx')
        expect(rewritten.character.ccAssets[0].uri).toBe('assets/account-card.png')
        expect(payload.character.image).toBe('assets/local-main.png')
        expect(payload.character.emotionImages[0][1]).toBe('assets/local-neutral.png')
        expect(payload.character.additionalAssets[0][1]).toBe('assets/local-prop.png')
        expect(payload.character.vits.files.model).toBe('assets/local-model.onnx')
        expect(payload.character.ccAssets[0].uri).toBe('assets/local-card.png')
    })

    it('leaves non-character cold storage payloads unchanged', () => {
        const payload = { message: [{ data: 'assets/local.png' }] }

        expect(replaceColdStoragePayloadResources(payload, {
            'assets/local.png': 'assets/account.png',
        })).toBe(payload)
    })
})
