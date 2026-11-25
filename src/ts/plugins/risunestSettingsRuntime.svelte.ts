import { alertError } from '../alert'
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

// In the order they run: the text colors follow the scheme's light or dark type, so the scheme goes first.
const settingEffectRunners: readonly (readonly [SettingEffect, () => Promise<void>])[] = [
    ['language', async () => (await import('src/lang')).changeLanguage(DBState.db.language)],
    ['colorScheme', async () => (await import('../gui/colorscheme')).updateColorScheme()],
    ['textTheme', async () => (await import('../gui/colorscheme')).updateTextThemeAndCSS()],
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
