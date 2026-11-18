import { get } from 'svelte/store'
import { DBState, selectedCharID } from './stores.svelte'
import { getModuleToggles } from './process/modules'
import { parseToggleSyntax, type sidebarToggle, type sidebarToggleGroup } from './util'

/** Toggle definitions shown in the sidebar: global prompt toggles, enabled modules, and the selected character's own toggles. */
export function currentToggleDefinitions(): string {
    const character = DBState.db.characters[get(selectedCharID)]
    const own = !!character ? (character.customModuleToggle ?? '') : ''
    return `${DBState.db.customPromptTemplateToggle ?? ''}\n${getModuleToggles()}\n${own}`
}

/** `toggle_` variable keys of the definitions that hold a value (switches, selects, text fields). */
export function currentToggleKeys(): string[] {
    return parseToggleSyntax(currentToggleDefinitions())
        .filter(
            (toggle) =>
                toggle.key &&
                toggle.type !== 'group' &&
                toggle.type !== 'groupEnd' &&
                toggle.type !== 'divider' &&
                toggle.type !== 'caption',
        )
        .map((toggle) => `toggle_${toggle.key}`)
}

/** Puts the toggles between a group and its groupEnd into that group, as the sidebar shows them. */
export function groupSidebarToggles(toggles: readonly sidebarToggle[]): sidebarToggle[] {
    const grouped: sidebarToggle[] = []
    let group: sidebarToggleGroup | null = null
    for (const toggle of toggles) {
        if (toggle.type === 'group') {
            group = { ...toggle, children: [] }
            grouped.push(group)
        } else if (toggle.type === 'groupEnd') {
            group = null
        } else if (group) {
            group.children.push(toggle)
        } else {
            grouped.push(toggle)
        }
    }
    return grouped
}
