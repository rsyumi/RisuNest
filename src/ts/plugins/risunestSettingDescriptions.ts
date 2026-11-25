import type { SettingItem } from '../setting/types'

// What the settings screen tells the user about a root key: the label, help, range and value
// list of the control bound to it. Text is resolved in the app's current language on every call.

export interface RisunestSettingDescription {
    label?: string
    help?: string
    min?: number
    max?: number
    step?: number
    options?: { value: string | number; label?: string }[]
}

export interface RisunestSettingTextResolver {
    /** The label the control shows. */
    label(item: SettingItem): string
    /** A language table entry by dotted path. */
    text(path: string): unknown
}

const controlTypes: ReadonlySet<SettingItem['type']> = new Set([
    'check', 'text', 'number', 'textarea', 'slider', 'select', 'segmented', 'color',
])

function nonEmptyText(value: unknown): string | undefined {
    return typeof value === 'string' && value.length > 0 ? value : undefined
}

function describeItem(item: SettingItem, resolver: RisunestSettingTextResolver): RisunestSettingDescription {
    const description: RisunestSettingDescription = {}
    const label = nonEmptyText(resolver.label(item))
    if (label) description.label = label
    if (item.helpKey) {
        // As the help button resolves it: a dotted key is a language path, any other a help entry.
        const help = nonEmptyText(resolver.text(item.helpKey.includes('.') ? item.helpKey : `help.${item.helpKey}`))
        if (help) description.help = help
    }
    for (const field of ['min', 'max', 'step'] as const) {
        const value = item.options?.[field]
        if (typeof value === 'number' && Number.isFinite(value)) description[field] = value
    }
    const choices = item.type === 'select' ? item.options?.selectOptions
        : item.type === 'segmented' ? item.options?.segmentOptions
            : undefined
    if (choices?.length) {
        description.options = choices.map((choice) => {
            const translated = choice.labelKey ? resolver.text(choice.labelKey) : undefined
            const optionLabel = nonEmptyText(translated) ?? nonEmptyText(choice.label)
            return optionLabel === undefined ? { value: choice.value } : { value: choice.value, label: optionLabel }
        })
    }
    return description
}

/**
 * Describes each key of `keys` that a control binds as a plain root key, from the first such
 * control. Controls bound through a nested path or a custom accessor describe nothing.
 * Visibility conditions are ignored, so value lists are whole.
 */
export function describeSettingItems(
    groups: readonly (readonly SettingItem[])[],
    keys: ReadonlySet<string>,
    resolver: RisunestSettingTextResolver,
): Map<string, RisunestSettingDescription> {
    const descriptions = new Map<string, RisunestSettingDescription>()
    const visit = (items: readonly SettingItem[]) => {
        for (const item of items) {
            const key = item.bindKey
            if (typeof key === 'string' && keys.has(key) && !descriptions.has(key) && controlTypes.has(item.type)
                && item.bindPath === undefined && !item.getValue && !item.setValue) {
                descriptions.set(key, describeItem(item, resolver))
            }
            if (item.options?.children) visit(item.options.children)
        }
    }
    for (const items of groups) visit(items)
    return descriptions
}

let definitions: Promise<{ groups: readonly (readonly SettingItem[])[]; resolver: RisunestSettingTextResolver }> | undefined

// The settings screens' own definitions, loaded on first use so the plugin host does not import them at startup.
export function loadSettingDefinitions() {
    if (definitions) return definitions
    const loading = Promise.all([
        import('../setting/accessibilitySettingsData'),
        import('../setting/advancedSettingsData'),
        import('../setting/botSettingsParamsData'),
        import('../setting/chatFormatSettingsData'),
        import('../setting/displaySettingsData.svelte'),
        import('../setting/languageSettingsData.svelte'),
        import('../setting/risuNestSettingsData'),
        import('../setting/utils'),
    ]).then(([accessibility, advanced, botParameters, chatFormat, display, languageSettings, risuNest, utils]) => ({
        groups: [
            accessibility.accessibilitySettingsItems,
            advanced.advancedSettingsItems,
            botParameters.allBasicParameterItems,
            chatFormat.chatFormatSettingsItems,
            display.displayThemeSettingsItems,
            display.displaySizeSettingsItems,
            display.displayOtherSettingsItems,
            languageSettings.languageSettingsItems,
            risuNest.risuNestUiSettingsItems,
            risuNest.risuNestStreamingSettingsItems,
            risuNest.risuNestInlaySettingsItems,
        ],
        resolver: { label: utils.getLabel, text: utils.resolveLanguagePath },
    }))
    definitions = loading
    loading.catch(() => {
        if (definitions === loading) definitions = undefined
    })
    return loading
}

export async function loadRisunestSettingDescriptions(
    keys: ReadonlySet<string>,
): Promise<Map<string, RisunestSettingDescription>> {
    const { groups, resolver } = await loadSettingDefinitions()
    return describeSettingItems(groups, keys, resolver)
}
