import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

import { decodeColdStoragePayload } from '../../../process/coldstorageData'
import { canonicalSha256 } from './canonicalCompatibility'
import { roadmap14Corpus, roadmap14Payloads } from './losslessCorpus'

describe('roadmap 14 lossless corpus', () => {
    it('covers rich ordered database semantics without using production serializers', () => {
        const { database } = roadmap14Corpus
        const normalCharacter = database.characters.find((character) => character.type !== 'group')
        const group = database.characters.find((character) => character.type === 'group')
        const completeChat = normalCharacter?.chats[0]
        const completeMessage = completeChat?.message[0]

        expect(normalCharacter).toMatchObject({
            globalLore: [{ useRegex: true }],
            customscript: [{ flag: 'g' }],
            virtualscript: expect.stringContaining('return'),
            modules: ['module-main', 'module-main'],
        })
        expect(normalCharacter?.triggerscript).toHaveLength(1)
        expect(group).toMatchObject({
            characters: ['character-main', 'character-main', 'character-known-missing'],
        })
        expect(completeChat).toMatchObject({
            sdData: '',
            supaMemoryData: '',
            lastMemory: '',
            suggestMessages: [],
            isStreaming: false,
            modules: ['module-main', 'module-main'],
            bookmarks: ['message-complete', 'message-complete'],
            useLocallySetGlobalVariables: false,
        })
        expect(completeMessage).toMatchObject({
            generationInfo: {
                inputTokens: 0,
                outputTokens: 0,
                stageTiming: { stage1: 0, stage2: 2, stage3: 3, stage4: 4 },
            },
            promptInfo: {
                promptToggles: [{ key: 'empty', value: '' }],
            },
            otherUser: false,
            disabled: false,
            isComment: false,
        })
        expect(database.botPresets.map((preset) => preset.name)).toEqual([
            'Preset Zeta',
            'Preset Alpha',
        ])
        expect(database.modules.map((module) => module.id)).toEqual([
            'module-main',
            'module-secondary',
        ])
        expect(database.personas.map((persona) => persona.id)).toEqual([
            'persona-main',
            'persona-secondary',
        ])
        expect(database.loadouts.map((loadout) => loadout.name)).toEqual([
            'Loadout Zeta',
            'Loadout Alpha',
        ])
        expect(Object.keys(database.pluginCustomStorage)).toEqual([
            '2',
            '10',
            '4294967294',
            '01',
            '4294967295',
            '__proto__',
            '',
            'unicode-한국어',
        ])
        expect(database.pluginCustomStorage).toMatchObject({
            '01': 0,
            '4294967295': false,
            '': null,
            '__proto__': { empty: '', nestedUnknown: { enabled: false } },
        })
        expect((database as unknown as { roadmap14Unknown: unknown }).roadmap14Unknown)
            .toEqual({ emptyObject: {}, emptyArray: [], undefinedValue: undefined })
    })

    it('pins deterministic ordinary assets, Inlays, cold payloads, and the Rust fixture hash', () => {
        const manifestPath = resolve(
            process.cwd(),
            'src/ts/storage/tests/roadmap14/fixtures/compatibility-manifest.json',
        )
        const rustFixturePath = resolve(
            process.cwd(),
            'src-tauri/fixtures/roadmap14-compatibility.json',
        )
        const manifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as {
            version: number
            canonicalSha256: string
            payloads: Record<string, string>
        }
        const rustFixture = JSON.parse(readFileSync(rustFixturePath, 'utf8')) as {
            version: number
            canonicalEncoding: string
            canonicalSha256: string
            paritySha256: string
        }

        expect(new Set(roadmap14Payloads.map((payload) => payload.category))).toEqual(
            new Set([
                'png',
                'jpeg',
                'webp',
                'gif',
                'svg',
                'avif',
                'unknown-image',
                'mp3',
                'wav',
                'ogg',
                'mp4',
                'webm',
                'json',
                'binary',
                'onnx',
                'extensionless',
                'uppercase-extension',
                'unknown-extension',
                'zero-length',
                'non-utf8',
                'inlay-image',
                'inlay-audio',
                'inlay-video',
                'inlay-signature',
                'cold-character',
                'cold-chat',
            ]),
        )
        expect(Object.keys(manifest.payloads)).toEqual(
            roadmap14Payloads.map((payload) => payload.key),
        )
        for (const payload of roadmap14Payloads) {
            expect(
                createHash('sha256').update(payload.bytes).digest('hex'),
                payload.key,
            ).toBe(manifest.payloads[payload.key])
        }
        expect(canonicalSha256(roadmap14Corpus.database)).toBe(manifest.canonicalSha256)
        expect(rustFixture).toEqual({
            version: manifest.version,
            canonicalEncoding: 'roadmap14-length-delimited-v1',
            canonicalSha256: manifest.canonicalSha256,
            paritySha256: 'df5969c838b7c30495d1ad1f891d1f761dc8a029724e1622b8063adff543fe5a',
        })
    })

    it('stores cold fixtures as production-decodable compressed JSON without side-channel values', async () => {
        const coldPayloads = roadmap14Payloads.filter((payload) => payload.kind === 'cold')
        const decoded = await Promise.all(
            coldPayloads.map((payload) => decodeColdStoragePayload(payload.bytes)),
        )

        expect(decoded[0]).toMatchObject({
            character: {
                chaId: 'character-main',
                roadmap14ColdUnknown: '{{inlay::inlay-signature}}',
            },
        })
        expect(decoded[1]).toMatchObject({
            message: [{ chatId: 'cold-message-main' }],
        })
        expect(coldPayloads.every((payload) => !('value' in payload))).toBe(true)
    })
})
