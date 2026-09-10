import { get } from 'svelte/store'
import { DBState, selectedCharID } from './stores.svelte'
import { getModuleToggles } from './process/modules'
import { parseToggleSyntax } from './util'

/** Toggle definitions shown in the sidebar: global prompt toggles, enabled modules, and the selected character's own toggles. */
export function currentToggleDefinitions(): string {
    const character = DBState.db.characters[get(selectedCharID)]
    const own = character?.type === 'character' ? (character.customModuleToggle ?? '') : ''
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
