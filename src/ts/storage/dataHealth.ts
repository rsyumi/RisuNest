/**
 * The diagnosis a native scan produces, and the grouping and reporting the screen needs. Nothing
 * here reads the store: the recovery shell mounts the same screen without a loaded library.
 */
import { localFileStamp } from '../localFileStamp'

export type DataHealthSeverity = 'blocking' | 'degraded' | 'informational'

export interface DataHealthFinding {
    code: string
    severity: DataHealthSeverity
    owner: { kind: string; id: string }
    locator: { sourcePath: string; occurrence: number } | null
    target: { kind: string; key: string } | null
    detail: string
    intentAction?: 'discard' | 'complete' | null
}

export interface DataHealthDeepProgress {
    cursor: string | null
    completedObjects: number
    totalObjects: number
    completedBytes: number
    totalBytes: number
    complete: boolean
}

export interface DataHealthCounts {
    blocking: number
    degraded: number
    informational: number
}

export interface DataHealthResult {
    revision: number
    scannedAt: number
    depth: 'quick' | 'deep'
    counts: DataHealthCounts
    items: DataHealthFinding[]
    omitted: number
    deep?: DataHealthDeepProgress
}

/** The stop the screen asked for. The native scan raises it so the loop ends without an alert. */
export const DATA_HEALTH_CANCELLED = 'data-health-scan-cancelled'

export function isDataHealthCancellation(error: unknown): boolean {
    if (typeof error === 'string') return error.includes(DATA_HEALTH_CANCELLED)
    if (error && typeof error === 'object') {
        const message = (error as { message?: unknown }).message
        if (typeof message === 'string')
            return message.includes(DATA_HEALTH_CANCELLED)
    }
    return false
}

/** Severity order for display: what blocks something comes before what merely looks broken. */
export const DATA_HEALTH_SEVERITIES: readonly DataHealthSeverity[] = [
    'blocking',
    'degraded',
    'informational',
]

/** Items one group lists before it summarises the rest. The saved report keeps everything. */
export const DATA_HEALTH_GROUP_LIMIT = 200

export interface DataHealthGroup {
    severity: DataHealthSeverity
    code: string
    total: number
    shown: DataHealthFinding[]
    hidden: number
}

/**
 * Groups by severity and then by code, keeping the order the scan produced inside a group so an
 * item stays where the reader last saw it.
 */
export function groupDataHealthFindings(
    items: readonly DataHealthFinding[],
    limit = DATA_HEALTH_GROUP_LIMIT,
): DataHealthGroup[] {
    const groups = new Map<string, DataHealthGroup>()
    for (const item of items) {
        const key = `${item.severity}\u0000${item.code}`
        let group = groups.get(key)
        if (!group) {
            group = {
                severity: item.severity,
                code: item.code,
                total: 0,
                shown: [],
                hidden: 0,
            }
            groups.set(key, group)
        }
        group.total += 1
        if (group.shown.length < limit) group.shown.push(item)
        else group.hidden += 1
    }
    return [...groups.values()].sort(
        (left, right) =>
            DATA_HEALTH_SEVERITIES.indexOf(left.severity) -
                DATA_HEALTH_SEVERITIES.indexOf(right.severity) ||
            left.code.localeCompare(right.code),
    )
}

export interface DataHealthReportOptions {
    /** Adds the display name of each owner. Off by default: a report can leave the device. */
    includeNames?: boolean
    resolveOwnerName?: (kind: string, id: string) => string | null
}

function reportLine(
    item: DataHealthFinding,
    options: DataHealthReportOptions,
): string {
    const parts = [item.severity, item.code, `${item.owner.kind}:${item.owner.id}`]
    const name = options.includeNames
        ? options.resolveOwnerName?.(item.owner.kind, item.owner.id)
        : null
    if (name) parts.push(`name=${name}`)
    if (item.locator)
        parts.push(`at=${item.locator.sourcePath}#${item.locator.occurrence}`)
    if (item.target) parts.push(`target=${item.target.kind}:${item.target.key}`)
    parts.push(item.detail)
    return parts.join('\t')
}

/**
 * The text the copy and save actions produce. The default profile carries codes, locators,
 * targets, hashes and the validator's own message, and no record content.
 */
export function formatDataHealthReport(
    result: DataHealthResult,
    options: DataHealthReportOptions = {},
): string {
    const header = [
        `revision\t${result.revision}`,
        `scannedAt\t${new Date(result.scannedAt).toISOString()}`,
        `depth\t${result.depth}`,
        `blocking\t${result.counts.blocking}`,
        `degraded\t${result.counts.degraded}`,
        `informational\t${result.counts.informational}`,
        `omitted\t${result.omitted}`,
    ]
    if (result.deep)
        header.push(
            `deepObjects\t${result.deep.completedObjects}/${result.deep.totalObjects}`,
            `deepBytes\t${result.deep.completedBytes}/${result.deep.totalBytes}`,
            `deepComplete\t${result.deep.complete}`,
        )
    header.push(`names\t${options.includeNames ? 'included' : 'masked'}`)
    return [
        ...header,
        '',
        ...result.items.map((item) => reportLine(item, options)),
    ].join('\n')
}

export function dataHealthReportFileName(result: DataHealthResult): string {
    return `risunest-data-health-${localFileStamp(result.scannedAt)}.txt`
}

/** Fraction of the deep pass that is done, by bytes, or null before a deep scan has started. */
export function dataHealthDeepFraction(
    result: DataHealthResult | null,
): number | null {
    const deep = result?.deep
    if (!deep) return null
    if (deep.complete) return 1
    if (deep.totalBytes > 0)
        return Math.min(1, deep.completedBytes / deep.totalBytes)
    if (deep.totalObjects > 0)
        return Math.min(1, deep.completedObjects / deep.totalObjects)
    return 0
}

/** What a repair may change, mirroring the native vocabulary. */
export type RepairAction =
    | { action: 'select-first-conversation'; character_id: string }
    | { action: 'select-first-preset' }
    | { action: 'select-first-persona' }
    | { action: 'assign-embedded-module-id'; persona_id: string }
    | { action: 'drop-reference'; owner: { kind: string; id: string }; sourcePath: string; occurrence: number }
    | { action: 'drop-alias'; kind: string; key: string }
    | { action: 'adopt-stored-payload'; kind: string; key: string }
    | { action: 'normalize-records'; table: string }
    | { action: 'keep-single-record'; table: string }
    | { action: 'recover-orphans'; table: string }

export interface RepairCandidate {
    id: string
    action: RepairAction
    finding: number
    preferred: boolean
    discards: boolean
}

export interface RepairPreview {
    selected: RepairCandidate[]
    answered: number
    remaining: number
    droppedReferences: number
    droppedAliases: number
    discarding: string[]
    tables: string[]
    proposesSnapshot: boolean
}

export interface RepairApplied {
    revision: number
    journalId: string
    snapshot: string | null
    result: DataHealthResult
}

export interface RepairUndone {
    revision: number
    skipped: string[]
    result: DataHealthResult
}

export interface RepairJournalSummary {
    id: string
    createdAt: number
    fromRevision: number
    toRevision: number
    changes: number
    heldObjects: number
    current: boolean
}

/** The choices a fresh diagnosis starts with: what the planner marks as preferred. */
export function preferredRepairSelection(
    candidates: readonly RepairCandidate[],
): string[] {
    return candidates
        .filter((candidate) => candidate.preferred)
        .map((candidate) => candidate.id)
}

/**
 * Selecting one candidate deselects the others that answer the same finding, so a finding never
 * carries two contradictory changes into the same repair.
 */
export function toggleRepairSelection(
    candidates: readonly RepairCandidate[],
    selection: readonly string[],
    id: string,
): string[] {
    const chosen = candidates.find((candidate) => candidate.id === id)
    if (!chosen) return [...selection]
    if (selection.includes(id))
        return selection.filter((selected) => selected !== id)
    const conflicting = new Set(
        candidates
            .filter((candidate) => candidate.finding === chosen.finding)
            .map((candidate) => candidate.id),
    )
    return [...selection.filter((selected) => !conflicting.has(selected)), id]
}

/**
 * One answer for every finding, the preferred one where the planner marks it. This is what the
 * select-all control chooses, so it never carries two answers to the same finding.
 */
export function allRepairSelection(
    candidates: readonly RepairCandidate[],
): string[] {
    const chosen = new Map<number, RepairCandidate>()
    for (const candidate of candidates) {
        const existing = chosen.get(candidate.finding)
        if (!existing || (candidate.preferred && !existing.preferred))
            chosen.set(candidate.finding, candidate)
    }
    return [...chosen.values()].map((candidate) => candidate.id)
}

/** Candidates grouped by the finding they answer, in diagnosis order. */
export function repairChoicesByFinding(
    candidates: readonly RepairCandidate[],
): Map<number, RepairCandidate[]> {
    const grouped = new Map<number, RepairCandidate[]>()
    for (const candidate of candidates) {
        const existing = grouped.get(candidate.finding)
        if (existing) existing.push(candidate)
        else grouped.set(candidate.finding, [candidate])
    }
    return grouped
}
