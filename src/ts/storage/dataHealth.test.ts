import { describe, expect, it } from 'vitest'
import {
    dataHealthDeepFraction,
    dataHealthReportFileName,
    formatDataHealthReport,
    groupDataHealthFindings,
    isDataHealthCancellation,
    type DataHealthFinding,
    type DataHealthResult,
} from './dataHealth'

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
