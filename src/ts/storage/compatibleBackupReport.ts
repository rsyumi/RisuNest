import type { NativeCompatibilityReport } from './nativeFileJobs'

export interface CompatibilityBackupReportLabels {
    report: string
    preserved: string
    converted: string
    excluded: string
    unknown: string
    items: string
    bytes: string
    conversations: string
    none: string
    otherCategory: string
    scopeNote: string
    categories: Readonly<Record<string, string>>
}

/** Keep the native decimal counts exact, including values larger than 2^53. */
function formatCount(value: string | null, unknown: string): string {
    return value !== null && /^\d+$/.test(value)
        ? value.replace(/\B(?=(\d{3})+(?!\d))/g, ',')
        : unknown
}

export function formatCompatibilityBackupReport(
    report: NativeCompatibilityReport,
    labels: CompatibilityBackupReportLabels,
): string {
    const target = report.target === 'risuai' ? 'RisuAI' : 'PocketRisu'
    const sections = (['preserved', 'converted', 'excluded'] as const).map(
        (section) => {
            const rows = report[section].map((item) => {
                const category = Object.hasOwn(labels.categories, item.code)
                    ? labels.categories[item.code]
                    : labels.otherCategory
                return `${category}\n  ${labels.items}: ${formatCount(item.items, labels.unknown)} · ${labels.bytes}: ${formatCount(item.bytes, labels.unknown)} B\n  ${labels.conversations}: ${formatCount(item.affectedConversations, labels.unknown)}`
            })
            return `${labels[section]}\n${rows.length ? rows.join('\n') : labels.none}`
        },
    )
    return `${labels.report} (${target})\n\n${sections.join('\n\n')}\n\n${labels.scopeNote}`
}
