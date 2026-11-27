import { alertError } from '../alert'
import { safeStructuredClone } from '../safeStructuredClone'
import { DBState } from '../stores.svelte'
import { commitPreparedUnitIntent, flushPendingDataLocally } from '../storage/persistentDataRuntime.svelte'
import { loadRisunestSettingDescriptions, risunestSettingActionChoices } from './risunestSettingDescriptions'
import type { RisunestSettingsDependencies } from './risunestSettingsAccess'

// The app side of the settings methods. Reads use the working copy the settings screens bind,
// which keeps the selected preset and persona as indexes where the store keeps their IDs.

type SettingEffect = 'language' | 'colorScheme' | 'textTheme' | 'guiSize' | 'animation' | 'fullScreen'

/** What the settings screens do after a change to a key, besides storing it. */
export const risunestSettingEffects: Readonly<Record<string, readonly SettingEffect[]>> = {
    language: ['language'],
    colorScheme: ['colorScheme', 'textTheme'],
    textTheme: ['textTheme'],
    customTextTheme: ['textTheme'],
    font: ['textTheme'],
    customFont: ['textTheme'],
    customCSS: ['textTheme'],
    textAreaSize: ['guiSize'],
    textAreaTextSize: ['guiSize'],
    sideBarSize: ['guiSize'],
    animationSpeed: ['animation'],
    fullScreen: ['fullScreen'],
}

// In the order they run: the text colors follow the scheme's light or dark type, so the scheme goes first.
const settingEffectRunners: readonly (readonly [SettingEffect, () => Promise<void>])[] = [
    ['language', async () => (await import('src/lang')).changeLanguage(DBState.db.language)],
    ['colorScheme', async () => (await import('../gui/colorscheme')).updateColorScheme()],
    ['textTheme', async () => {
        // A start that leaves the theme off keeps the text theme and custom CSS off, as a received change does.
        if (!(await import('../gui/receivedDisplaySettings')).isThemeExcluded()) (await import('../gui/colorscheme')).updateTextThemeAndCSS()
    }],
    ['guiSize', async () => (await import('../gui/guisize')).updateGuisize()],
    ['animation', async () => (await import('../gui/animation')).updateAnimationSpeed()],
    ['fullScreen', async () => (await import('../util')).changeFullscreen()],
]

async function applySettingEffects(keys: readonly string[]) {
    const effects = new Set(keys.flatMap((key) => Object.hasOwn(risunestSettingEffects, key) ? risunestSettingEffects[key] : []))
    for (const [effect, run] of settingEffectRunners) {
        if (!effects.has(effect)) continue
        try {
            await run()
        } catch (error) {
            // The value is stored, so the write still applied. The settings screens show the
            // same failure as an app error.
            alertError(error instanceof Error ? error : String(error))
        }
    }
}

type ColorSchemeModule = typeof import('../gui/colorscheme')

// The colors the app paints with follow the scheme name, and the custom colors while the custom
// scheme is chosen, as the settings screen sets them together. A written `colorScheme` wins.
// `stored` is the root the changes queued before this one left.
function colorSchemeFollowing(values: readonly (readonly [string, unknown])[], stored: Record<string, unknown>,
    { colorSchemePresets, defaultColorScheme }: ColorSchemeModule): [string, unknown][] {
    const written = new Map(values)
    if (written.has('colorScheme')) return []
    const name = written.has('colorSchemeName') ? written.get('colorSchemeName') : stored['colorSchemeName']
    if (!written.has('colorSchemeName') && !(written.has('customColorScheme') && name === 'custom')) return []
    const scheme = name === 'custom'
        ? (written.has('customColorScheme') ? written.get('customColorScheme') : stored['customColorScheme']) ?? defaultColorScheme
        : colorSchemePresets[name as keyof typeof colorSchemePresets]
    return [['colorScheme', safeStructuredClone(scheme)]]
}

export const productionRisunestSettingsDependencies: RisunestSettingsDependencies = {
    async findUnknownValue(values) {
        const action = values.find(([key, value]) => Object.hasOwn(risunestSettingActionChoices, key) && risunestSettingActionChoices[key].includes(value))
        if (action) return action[0]
        // A persona is selected by its place in the list, so the place has to hold one.
        const persona = values.find(([key]) => key === 'selectedPersona')
        if (persona && !DBState.db.personas[persona[1] as number]?.id) return 'selectedPersona'
        const name = values.find(([key]) => key === 'colorSchemeName')
        if (!name || name[1] === 'custom') return null
        const { colorSchemeList } = await import('../gui/colorscheme')
        return (colorSchemeList as readonly unknown[]).includes(name[1]) ? null : 'colorSchemeName'
    },
    flushPendingData: (reason) => flushPendingDataLocally(reason),
    readRootValues(keys) {
        const database = DBState.db as unknown as Record<string, unknown>
        const values: Record<string, unknown> = {}
        for (const key of keys) {
            const value = database[key]
            if (value !== undefined) values[key] = $state.snapshot(value)
        }
        return values
    },
    async commitRootValues(reason, values) {
        const colors = values.some(([key]) => key === 'colorSchemeName' || key === 'customColorScheme')
            ? await import('../gui/colorscheme') : null
        let committed: readonly (readonly [string, unknown])[] = values
        await commitPreparedUnitIntent(reason, async (reader) => {
            // Decided in the save queue, so a change called earlier is already in the stored root.
            committed = colors
                ? [...values, ...colorSchemeFollowing(values, (await reader.readRoot()).value as unknown as Record<string, unknown>, colors)]
                : values
            return {
                unitMutations: committed.map(([key, value]) => ({ key: JSON.stringify(['root', key]), type: 'set' as const, value })),
                conversations: [],
            }
        })
        await applySettingEffects(committed.map(([key]) => key))
    },
    loadDescriptions: (keys) => loadRisunestSettingDescriptions(keys),
}
