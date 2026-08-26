import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

import { canonicalSha256 } from './canonicalCompatibility'
import {
    roadmap14Cards,
    roadmap14Corpus,
    roadmap14ExpectedMissing,
    roadmap14Payloads,
} from './losslessCorpus'
import {
    buildReferenceGraph,
    summarizeReferenceGraph,
    validatePayloadInventory,
} from './referenceGraph'

function graphInput() {
    return {
        database: roadmap14Corpus.database,
        payloads: roadmap14Payloads,
        coldPayloads: roadmap14Corpus.coldPayloads,
        cards: roadmap14Cards,
        expectedMissing: roadmap14ExpectedMissing,
    }
}

describe('roadmap 14 reference graph', () => {
    it('emits every source occurrence with stable resolution diagnostics', async () => {
        const graph = await buildReferenceGraph(graphInput())
        const summary = summarizeReferenceGraph(graph)

        expect(graph[0]).toEqual({
            owner: { kind: 'root', id: 'database' },
            path: '$.userIcon',
            occurrence: 0,
            target: {
                kind: 'asset',
                key: 'assets/root/user-icon.png',
                metadata: { field: 'userIcon' },
            },
            status: 'present',
        })
        expect(
            graph.filter((edge) => edge.target.kind === 'asset'
                && edge.target.key === 'assets/shared/shared.bin'),
        ).toHaveLength(8)
        expect(
            graph.filter((edge) => edge.owner.id === 'group-main'
                && edge.target.kind === 'character'
                && edge.target.key === 'character-main'),
        ).toHaveLength(2)
        expect(
            graph.filter((edge) => edge.target.kind === 'inlay'
                && edge.target.key === 'inlay-image'),
        ).toHaveLength(6)
        expect(
            graph.some((edge) => edge.owner.kind === 'cold'
                && edge.target.kind === 'inlay'
                && edge.target.key === 'inlay-audio'),
        ).toBe(true)
        expect(
            graph.some((edge) => edge.owner.kind === 'card'
                && edge.target.kind === 'card'
                && edge.target.key === 'card-secondary'),
        ).toBe(true)

        expect(summary.unexpectedMissing).toEqual([])
        expect(summary.expectedMissing.map((edge) => [edge.target.kind, edge.target.key]))
            .toEqual(expect.arrayContaining([
                ['asset', 'assets/missing/known-missing.dat'],
                ['asset', 'assets/missing/module.dat'],
                ['inlay', 'inlay-known-missing'],
                ['character', 'character-known-missing'],
                ['card', 'card-known-missing'],
            ]))
        expect(summary.external.map((edge) => edge.target.key)).toEqual(
            expect.arrayContaining([
                'https://example.invalid/external.png',
                'data:image/png;base64,AA==',
            ]),
        )
        expect(summary.invalid).toHaveLength(1)
        expect(summary.invalid[0]).toMatchObject({
            owner: { kind: 'card', id: 'card-secondary' },
            target: { kind: 'module', key: '' },
        })

        const edgesByOwner = new Map<string, typeof graph>()
        for (const edge of graph) {
            const ownerKey = `${edge.owner.kind}:${edge.owner.id}`
            const edges = edgesByOwner.get(ownerKey) ?? []
            edges.push(edge)
            edgesByOwner.set(ownerKey, edges)
        }
        for (const edges of edgesByOwner.values()) {
            expect(edges.map((edge) => edge.occurrence)).toEqual(
                Array.from({ length: edges.length }, (_, index) => index),
            )
        }

        const roundTripGraph = await buildReferenceGraph(structuredClone(graphInput()))
        expect(roundTripGraph).toEqual(graph)
    })

    it('separates newly dangling targets from the checked-in missing baseline', async () => {
        const input = graphInput()
        const withoutSharedAsset = {
            ...input,
            payloads: input.payloads.filter(
                (payload) => payload.key !== 'assets/shared/shared.bin',
            ),
        }
        const summary = summarizeReferenceGraph(
            await buildReferenceGraph(withoutSharedAsset),
        )

        expect(summary.unexpectedMissing).not.toEqual([])
        expect(new Set(summary.unexpectedMissing.map((edge) => edge.target.key))).toEqual(
            new Set(['assets/shared/shared.bin']),
        )
        expect(summary.expectedMissing).not.toEqual([])
    })

    it('emits the selected conversation binding from each nonempty character chatPage', async () => {
        const graph = await buildReferenceGraph(graphInput())

        expect(graph).toContainEqual({
            owner: { kind: 'character', id: 'character-main' },
            path: '$.chatPage',
            occurrence: expect.any(Number),
            target: {
                kind: 'conversation',
                key: 'chat-complete',
                metadata: { characterId: 'character-main', index: 0 },
            },
            status: 'present',
        })
    })

    it('resolves conversation folders only within the owning character', async () => {
        const input = structuredClone(graphInput())
        const mainCharacter = input.database.characters[0]
        const otherCharacter = input.database.characters[1]
        otherCharacter.chatFolders.push({
            id: 'other-character-folder',
            name: 'Other folder',
            color: '',
            folded: false,
        })
        mainCharacter.chats[0].folderId = 'other-character-folder'

        const graph = await buildReferenceGraph(input)

        expect(graph).toContainEqual({
            owner: { kind: 'conversation', id: 'character-main/chat-complete' },
            path: '$.folderId',
            occurrence: expect.any(Number),
            target: {
                kind: 'folder',
                key: 'other-character-folder',
                metadata: { characterId: 'character-main' },
            },
            status: 'unexpected-missing',
        })
    })

    it('recursively scans inlays in stored character-order and loadout subtrees', async () => {
        const input = structuredClone(graphInput())
        const folder = input.database.characterOrder[0] as unknown as Record<string, unknown>
        const loadout = input.database.loadouts[0] as unknown as Record<string, unknown>
        folder.roadmap14Unknown = { nestedInlay: '{{inlay::inlay-audio}}' }
        loadout.roadmap14Unknown = { nestedInlay: '{{inlay::inlay-signature}}' }

        const graph = await buildReferenceGraph(input)

        expect(graph).toEqual(expect.arrayContaining([
            expect.objectContaining({
                owner: { kind: 'folder', id: 'folder-main' },
                path: '$.roadmap14Unknown.nestedInlay',
                target: expect.objectContaining({ kind: 'inlay', key: 'inlay-audio' }),
                status: 'present',
            }),
            expect.objectContaining({
                owner: { kind: 'loadout', id: 'Loadout Zeta' },
                path: '$.roadmap14Unknown.nestedInlay',
                target: expect.objectContaining({ kind: 'inlay', key: 'inlay-signature' }),
                status: 'present',
            }),
        ]))
    })

    it('emits invalid edges for empty required asset tuple targets', async () => {
        const input = structuredClone(graphInput())
        input.database.characters[0].additionalAssets[0][1] = ''

        const graph = await buildReferenceGraph(input)

        expect(graph).toContainEqual({
            owner: { kind: 'character', id: 'character-main' },
            path: '$.additionalAssets[0][1]',
            occurrence: expect.any(Number),
            target: {
                kind: 'asset',
                key: '',
                metadata: { name: 'Shared asset', ext: 'bin' },
            },
            status: 'invalid',
        })
    })

    it('matches the independent checked-in reference graph hash', async () => {
        const manifestPath = resolve(
            process.cwd(),
            'src/ts/storage/tests/roadmap14/fixtures/compatibility-manifest.json',
        )
        const manifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as {
            referenceGraphSha256: string
        }

        expect(canonicalSha256(await buildReferenceGraph(graphInput()))).toBe(
            manifest.referenceGraphSha256,
        )
    })

    it('compares payload bytes against literal checked-in SHA-256 values', () => {
        const manifestPath = resolve(
            process.cwd(),
            'src/ts/storage/tests/roadmap14/fixtures/compatibility-manifest.json',
        )
        const manifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as {
            payloadCount: number
            payloads: Record<string, string>
        }

        expect(manifest.payloadCount).toBe(32)
        expect(validatePayloadInventory(
            roadmap14Payloads,
            manifest.payloads,
            manifest.payloadCount,
        )).toEqual({
            valid: true,
            missing: [],
            unexpected: [],
            mismatches: [],
            duplicates: [],
            cardinality: { expected: 32, actual: 32 },
        })

        const changedPayloads = roadmap14Payloads.map((payload, index) => index === 0
            ? { ...payload, bytes: new Uint8Array([...payload.bytes, 0]) }
            : payload)
        expect(validatePayloadInventory(
            changedPayloads,
            manifest.payloads,
            manifest.payloadCount,
        )).toMatchObject({
            valid: false,
            missing: [],
            unexpected: [],
            mismatches: [{ key: 'assets/characters/main.PNG' }],
        })

        const duplicatedPayloads = [...roadmap14Payloads, roadmap14Payloads[0]]
        expect(validatePayloadInventory(
            duplicatedPayloads,
            manifest.payloads,
            manifest.payloadCount,
        )).toMatchObject({
            valid: false,
            duplicates: [{ key: 'assets/characters/main.PNG', count: 2 }],
            cardinality: { expected: 32, actual: 33 },
        })
    })
})
