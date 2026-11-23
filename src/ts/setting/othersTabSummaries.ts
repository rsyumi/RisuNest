import { language } from 'src/lang'
import { parseToggleSyntax } from 'src/ts/util'

function countText(count: number): string {
    return count > 0 ? language.risuNest.promptOptions.count.replace('{0}', String(count)) : ''
}

/** The number of rows in a list, or '' when it has none. */
export function listSummary(rows: readonly unknown[] | undefined): string {
    return countText(rows?.length ?? 0)
}

/** Chosen custom flags, counted only while custom flags are enabled. */
export function customFlagsSummary(enabled: boolean | undefined, flags: readonly unknown[] | undefined): string {
    return enabled ? countText(new Set(flags ?? []).size) : ''
}

/** Module IDs entered for module integration, read the way the prompt reads them. */
export function moduleIntegrationSummary(text: string | undefined): string {
    return countText(text ? text.split(',').map((id) => id.trim()).filter(Boolean).length : 0)
}

/** Names of the enabled model tools, in the order the tools accordion lists them. */
export function toolsSummary(tools: readonly string[] | undefined): string {
    const labels: Record<string, string> = { search: language.search }
    return Object.keys(labels).filter((tool) => tools?.includes(tool)).map((tool) => labels[tool]).join(', ')
}

/** Toggles that draw a control in the sidebar; groups, dividers and captions are not toggles. */
export function customTogglesSummary(text: unknown): string {
    if (typeof text !== 'string') return ''
    const kinds = new Set(['group', 'groupEnd', 'divider', 'caption'])
    return countText(parseToggleSyntax(text).filter((toggle) => !kinds.has(toggle.type ?? '')).length)
}
