import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

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
    it('emits every source occurrence with stable resolution diagnostics', () => {
        const graph = buildReferenceGraph(graphInput())
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

        const roundTripGraph = buildReferenceGraph(structuredClone(graphInput()))
        expect(roundTripGraph).toEqual(graph)
    })

    it('separates newly dangling targets from the checked-in missing baseline', () => {
        const input = graphInput()
        const withoutSharedAsset = {
            ...input,
            payloads: input.payloads.filter(
                (payload) => payload.key !== 'assets/shared/shared.bin',
            ),
        }
        const summary = summarizeReferenceGraph(buildReferenceGraph(withoutSharedAsset))

        expect(summary.unexpectedMissing).not.toEqual([])
        expect(new Set(summary.unexpectedMissing.map((edge) => edge.target.key))).toEqual(
            new Set(['assets/shared/shared.bin']),
        )
        expect(summary.expectedMissing).not.toEqual([])
    })

    it('compares payload bytes against literal checked-in SHA-256 values', () => {
        const manifestPath = resolve(
            process.cwd(),
            'src/ts/storage/tests/roadmap14/fixtures/compatibility-manifest.json',
        )
        const manifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as {
            payloads: Record<string, string>
        }

        expect(validatePayloadInventory(roadmap14Payloads, manifest.payloads)).toEqual({
            valid: true,
            missing: [],
            unexpected: [],
            mismatches: [],
        })

        const changedPayloads = roadmap14Payloads.map((payload, index) => index === 0
            ? { ...payload, bytes: new Uint8Array([...payload.bytes, 0]) }
            : payload)
        expect(validatePayloadInventory(changedPayloads, manifest.payloads)).toMatchObject({
            valid: false,
            missing: [],
            unexpected: [],
            mismatches: [{ key: 'assets/characters/main.PNG' }],
        })
    })
})
