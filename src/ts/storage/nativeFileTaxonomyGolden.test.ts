import { describe, expect, it, vi } from 'vitest'
import taxonomy from './tests/fixtures/nativeFileTaxonomyV1Golden.json'
import { ANDROID_SAF_HANDOFF_ID_PATTERNS } from './nativeFileJobRecovery'
import {
    consumeAndroidSpoolBatch,
    isAndroidNativeContentSpool,
    type AndroidSpoolReady,
} from './androidSafBridge'

// Characterization guard for the native export/import filename taxonomy that
// is independently hardcoded in Kotlin (SafFileBridge), Rust
// (native_file_jobs handoff cleanup, persistent_store export naming), and
// TypeScript. Each side pins itself to the same golden fixture; a drift on
// any side fails that side's suite.

const HANDOFF_KIND_TO_JOB_KIND: Record<
    string,
    keyof typeof ANDROID_SAF_HANDOFF_ID_PATTERNS
> = {
    'portable-backup': 'export-portable-backup',
    'legacy-backup': 'export-legacy-local-backup',
    'character-charx': 'export-character-charx',
    'character-card': 'export-character-card',
    'risu-module': 'export-risu-module',
}

function spoolSource(displayName: string): AndroidSpoolReady {
    return { token: 'token-1', displayName, bytes: 4, totalBytes: 4 }
}

describe('native file taxonomy golden fixture', () => {
    it('covers every recovery pattern with a fixture grammar and vice versa', () => {
        expect(
            [
                ...Object.values(HANDOFF_KIND_TO_JOB_KIND),
                'export-compatible-local-backup',
            ].sort(),
        ).toEqual(Object.keys(ANDROID_SAF_HANDOFF_ID_PATTERNS).sort())
        expect(
            taxonomy.managedHandoffs.map((grammar) => grammar.kind).sort(),
        ).toEqual(Object.keys(HANDOFF_KIND_TO_JOB_KIND).sort())
    })

    it('shares the legacy handoff grammar with target-specific compatible export', () => {
        expect(
            ANDROID_SAF_HANDOFF_ID_PATTERNS['export-compatible-local-backup']
                ?.source,
        ).toBe(
            ANDROID_SAF_HANDOFF_ID_PATTERNS['export-legacy-local-backup']
                ?.source,
        )
    })

    it('accepts every managed handoff grammar and captures the export id', () => {
        for (const grammar of taxonomy.managedHandoffs) {
            const pattern =
                ANDROID_SAF_HANDOFF_ID_PATTERNS[
                    HANDOFF_KIND_TO_JOB_KIND[grammar.kind]
                ]
            expect(pattern, grammar.kind).toBeDefined()
            for (const suffix of grammar.suffixes) {
                const name = `${grammar.prefix}${taxonomy.uuid}${suffix}`
                const path = `C:\\app\\${taxonomy.handoffDirectory.replaceAll('/', '\\')}\\${name}`
                expect(pattern!.exec(path)?.[1], name).toBe(taxonomy.uuid)
                expect(
                    pattern!.exec(
                        `/data/app/${taxonomy.handoffDirectory}/${name}`,
                    )?.[1],
                    name,
                ).toBe(taxonomy.uuid)
            }
        }
    })

    it('rejects malformed managed module names', () => {
        const pattern = ANDROID_SAF_HANDOFF_ID_PATTERNS['export-risu-module']!
        for (const name of taxonomy.rejectedModuleNames) {
            expect(
                pattern.exec(`/data/app/${taxonomy.handoffDirectory}/${name}`),
                name,
            ).toBeNull()
        }
    })

    it('rejects a grammar name against every other grammar', () => {
        for (const grammar of taxonomy.managedHandoffs) {
            const name = `${grammar.prefix}${taxonomy.uuid}${grammar.suffixes[0]}`
            for (const other of taxonomy.managedHandoffs) {
                if (other.kind === grammar.kind) continue
                const pattern =
                    ANDROID_SAF_HANDOFF_ID_PATTERNS[
                        HANDOFF_KIND_TO_JOB_KIND[other.kind]
                    ]!
                expect(
                    pattern.exec(`/x/${name}`),
                    `${name} vs ${other.kind}`,
                ).toBeNull()
            }
        }
    })

    it('routes database spool suffixes to restore and content suffixes to unsupported', async () => {
        const suffixes = [
            ...taxonomy.spoolSuffixes.database.map(
                (suffix) => [suffix, 'restore'] as const,
            ),
            ...taxonomy.spoolSuffixes.content.map(
                (suffix) => [suffix, 'unsupported'] as const,
            ),
        ]
        for (const [suffix, route] of suffixes) {
            const restore = vi.fn(async () => {})
            const unsupported = vi.fn()
            await consumeAndroidSpoolBatch(
                {
                    requestId: 'request-1',
                    ready: [spoolSource(`import${suffix}`)],
                    failures: [],
                },
                { restore, unsupported },
            )
            expect(restore.mock.calls.length, suffix).toBe(
                route === 'restore' ? 1 : 0,
            )
            expect(unsupported.mock.calls.length, suffix).toBe(
                route === 'unsupported' ? 1 : 0,
            )
        }
    })

    it('classifies exactly the content suffixes as native content spools', () => {
        for (const suffix of taxonomy.spoolSuffixes.content) {
            expect(
                isAndroidNativeContentSpool(spoolSource(`a${suffix}`)),
                suffix,
            ).toBe(true)
            expect(
                isAndroidNativeContentSpool(
                    spoolSource(`a${suffix.toUpperCase()}`),
                ),
                suffix,
            ).toBe(true)
        }
        for (const suffix of taxonomy.spoolSuffixes.database) {
            expect(
                isAndroidNativeContentSpool(spoolSource(`a${suffix}`)),
                suffix,
            ).toBe(false)
        }
        expect(isAndroidNativeContentSpool(spoolSource('a.txt'))).toBe(false)
    })

    it('keeps the database and content spool subsets disjoint', () => {
        const database = new Set(taxonomy.spoolSuffixes.database)
        for (const suffix of taxonomy.spoolSuffixes.content) {
            expect(database.has(suffix), suffix).toBe(false)
        }
    })
})
