import { DBState } from '../stores.svelte'
import { commitPersistentUnitIntent, flushPendingDataLocally } from '../storage/persistentDataRuntime.svelte'
import { loadRisunestSettingDescriptions } from './risunestSettingDescriptions'
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

async function applySettingEffects(keys: readonly string[]) {
    const effects = new Set(keys.flatMap((key) => Object.hasOwn(risunestSettingEffects, key) ? risunestSettingEffects[key] : []))
    if (effects.has('language')) (await import('src/lang')).changeLanguage(DBState.db.language)
    // The text colors follow the scheme's light or dark type, so the scheme goes first.
    if (effects.has('colorScheme')) (await import('../gui/colorscheme')).updateColorScheme()
    if (effects.has('textTheme')) (await import('../gui/colorscheme')).updateTextThemeAndCSS()
    if (effects.has('guiSize')) (await import('../gui/guisize')).updateGuisize()
    if (effects.has('animation')) (await import('../gui/animation')).updateAnimationSpeed()
    if (effects.has('fullScreen')) await (await import('../util')).changeFullscreen()
}

export const productionRisunestSettingsDependencies: RisunestSettingsDependencies = {
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
        await commitPersistentUnitIntent(reason, values.map(([key, value]) => ({
            key: JSON.stringify(['root', key]),
            type: 'set' as const,
            value,
        })))
        await applySettingEffects(values.map(([key]) => key))
    },
    loadDescriptions: (keys) => loadRisunestSettingDescriptions(keys),
}
