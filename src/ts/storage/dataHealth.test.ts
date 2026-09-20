import { describe, expect, it } from 'vitest'
import {
    preferredRepairSelection,
    repairChoicesByFinding,
    toggleRepairSelection,
    type RepairCandidate,
    dataHealthDeepFraction,
    dataHealthReportFileName,
    formatDataHealthReport,
    groupDataHealthFindings,
    isDataHealthCancellation,
    type DataHealthFinding,
    type DataHealthResult,
} from './dataHealth'
import { describeDataHealthFinding } from './dataHealthPresentation'

function finding(
    overrides: Partial<DataHealthFinding> = {},
): DataHealthFinding {
    return {
        code: 'reference-missing',
        severity: 'degraded',
        owner: { kind: 'character', id: 'char-1' },
        locator: { sourcePath: '$.image', occurrence: 0 },
        target: { kind: 'asset', key: 'assets/portrait.png' },
        detail: 'reference has no target in this library',
        ...overrides,
    }
}

const presentation = {
    rootSeparatedField: (field: string) => `root:${field}`,
    moduleAssetMissing: (module: string, ordinal: number) => `${module}:${ordinal}:asset`,
    conversationModuleMissing: (character: string, conversation: string) =>
        `${character}:${conversation}:module`,
    conversationMessageInlayMissing: (
        character: string,
        conversation: string,
        ordinal: number,
    ) => `${character}:${conversation}:${ordinal}:inlay`,
}

describe('describeDataHealthFinding', () => {
    it('describes a module asset by module name and human ordinal', () => {
        const item = finding({
            code: 'reference-invalid',
            owner: { kind: 'module', id: 'module-1' },
            locator: { sourcePath: '$.assets[0][1]', occurrence: 0 },
            target: { kind: 'asset', key: '' },
        })
        expect(
            describeDataHealthFinding(
                item,
                { ownerName: 'Weather' },
                presentation,
            ),
        ).toBe('Weather:1:asset')
    })

    it('describes a missing conversation module by character and chat names', () => {
        const item = finding({
            owner: { kind: 'conversation', id: 'character-1/chat-1' },
            locator: { sourcePath: '$.modules[0]', occurrence: 0 },
            target: { kind: 'module', key: 'missing-module' },
        })
        expect(
            describeDataHealthFinding(
                item,
                { characterName: 'Mari', conversationName: 'First chat' },
                presentation,
            ),
        ).toBe('Mari:First chat:module')
    })

    it('describes a broken message inlay by character, chat and message ordinal', () => {
        const item = finding({
            owner: { kind: 'conversation', id: 'character-1/chat-1' },
            locator: { sourcePath: '$.message[7].data', occurrence: 0 },
            target: { kind: 'inlay', key: 'missing-inlay' },
        })
        expect(
            describeDataHealthFinding(
                item,
                { characterName: 'Mari', conversationName: 'First chat' },
                presentation,
            ),
        ).toBe('Mari:First chat:8:inlay')
    })

    it('names the separated root field without exposing its value', () => {
        const item = finding({
            code: 'record-invalid',
            severity: 'blocking',
            owner: { kind: 'root', id: '' },
            locator: null,
            target: null,
            detail: 'portable root contains separated field: pluginStorageMeta',
        })
        expect(describeDataHealthFinding(item, null, presentation)).toBe(
            'root:pluginStorageMeta',
        )
    })
})

function result(overrides: Partial<DataHealthResult> = {}): DataHealthResult {
    const items = overrides.items ?? [finding()]
    return {
        revision: 12,
        scannedAt: Date.UTC(2026, 8, 15, 4, 5, 6),
        depth: 'quick',
        counts: { blocking: 0, degraded: items.length, informational: 0 },
        omitted: 0,
        ...overrides,
        items,
    }
}

describe('groupDataHealthFindings', () => {
    it('orders blocking groups before what only looks broken', () => {
        const groups = groupDataHealthFindings([
            finding({ code: 'object-unreferenced', severity: 'informational' }),
            finding(),
            finding({ code: 'record-invalid', severity: 'blocking' }),
        ])
        expect(groups.map((group) => group.severity)).toEqual([
            'blocking',
            'degraded',
            'informational',
        ])
    })

    it('counts every item but only carries the first ones into the list', () => {
        const items = Array.from({ length: 5 }, (_, index) =>
            finding({ owner: { kind: 'character', id: `char-${index}` } }),
        )
        const [group] = groupDataHealthFindings(items, 2)
        expect(group.total).toBe(5)
        expect(group.shown).toHaveLength(2)
        expect(group.hidden).toBe(3)
        expect(group.shown[0].owner.id).toBe('char-0')
    })
})

describe('formatDataHealthReport', () => {
    it('masks names by default and says so', () => {
        const report = formatDataHealthReport(result(), {
            resolveOwnerName: () => 'Mari',
        })
        expect(report).toContain('names\tmasked')
        expect(report).not.toContain('Mari')
        expect(report).toContain('character:char-1')
        expect(report).toContain('at=$.image#0')
        expect(report).toContain('target=asset:assets/portrait.png')
        expect(report).toContain('reference has no target in this library')
    })

    it('adds names only when the reader turns them on', () => {
        const report = formatDataHealthReport(result(), {
            includeNames: true,
            resolveOwnerName: () => 'Mari',
        })
        expect(report).toContain('names\tincluded')
        expect(report).toContain('name=Mari')
    })

    it('reports deep progress when a deep scan produced the result', () => {
        const report = formatDataHealthReport(
            result({
                depth: 'deep',
                deep: {
                    cursor: 'ab',
                    completedObjects: 3,
                    totalObjects: 4,
                    completedBytes: 30,
                    totalBytes: 40,
                    complete: false,
                },
            }),
        )
        expect(report).toContain('deepObjects\t3/4')
        expect(report).toContain('deepBytes\t30/40')
        expect(report).toContain('deepComplete\tfalse')
    })
})

describe('dataHealthReportFileName', () => {
    it('names the file after the moment of the scan', () => {
        expect(dataHealthReportFileName(result())).toBe(
            'risunest-data-health-2026-09-15T04-05-06Z.txt',
        )
    })
})

describe('dataHealthDeepFraction', () => {
    const deep = {
        cursor: null,
        completedObjects: 1,
        totalObjects: 4,
        completedBytes: 10,
        totalBytes: 40,
        complete: false,
    }

    it('has no fraction before a deep scan has started', () => {
        expect(dataHealthDeepFraction(result())).toBeNull()
        expect(dataHealthDeepFraction(null)).toBeNull()
    })

    it('measures by bytes, and falls back to object counts', () => {
        expect(dataHealthDeepFraction(result({ deep }))).toBeCloseTo(0.25)
        expect(
            dataHealthDeepFraction(
                result({ deep: { ...deep, totalBytes: 0, completedBytes: 0 } }),
            ),
        ).toBeCloseTo(0.25)
        expect(
            dataHealthDeepFraction(result({ deep: { ...deep, complete: true } })),
        ).toBe(1)
    })
})

describe('isDataHealthCancellation', () => {
    it('recognises the stop the screen asked for, and nothing else', () => {
        expect(
            isDataHealthCancellation({ message: 'data-health-scan-cancelled' }),
        ).toBe(true)
        expect(isDataHealthCancellation('data-health-scan-cancelled')).toBe(true)
        expect(isDataHealthCancellation(new Error('disk is full'))).toBe(false)
        expect(isDataHealthCancellation(undefined)).toBe(false)
    })
})

describe('repair selection', () => {
    const candidates: RepairCandidate[] = [
        {
            id: '0:drop-alias',
            action: { action: 'drop-alias', kind: 'asset', key: 'assets/a.png' },
            finding: 0,
            preferred: true,
            discards: true,
        },
        {
            id: '0:adopt-stored-payload',
            action: { action: 'adopt-stored-payload', kind: 'asset', key: 'assets/a.png' },
            finding: 0,
            preferred: false,
            discards: false,
        },
        {
            id: '1:settle-authority',
            action: { action: 'settle-authority', subject: 'asset' },
            finding: 1,
            preferred: true,
            discards: false,
        },
    ]

    it('starts from the choices the planner marked', () => {
        expect(preferredRepairSelection(candidates)).toEqual([
            '0:drop-alias',
            '1:settle-authority',
        ])
    })

    it('lets one finding carry only one answer', () => {
        const selection = toggleRepairSelection(
            candidates,
            ['0:drop-alias', '1:settle-authority'],
            '0:adopt-stored-payload',
        )
        expect(selection).toEqual(['1:settle-authority', '0:adopt-stored-payload'])
    })

    it('deselects what is already chosen and ignores what was never offered', () => {
        expect(
            toggleRepairSelection(candidates, ['0:drop-alias'], '0:drop-alias'),
        ).toEqual([])
        expect(
            toggleRepairSelection(candidates, ['0:drop-alias'], 'nothing'),
        ).toEqual(['0:drop-alias'])
    })

    it('groups the answers by the finding they belong to', () => {
        const grouped = repairChoicesByFinding(candidates)
        expect([...grouped.keys()]).toEqual([0, 1])
        expect(grouped.get(0)).toHaveLength(2)
    })
})
