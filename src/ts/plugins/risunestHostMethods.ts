import { createFloatingPluginWindow } from './floatingPluginWindow'
import { createRisunestCharacterAccess } from './risunestCharacterAccess'
import { createRisunestGlobalDataAccess } from './risunestGlobalDataAccess'
import type { RisunestHostContext } from './risunestHostContext'
import { checkRisunestLuaSyntax } from './risunestLuaSyntaxCheck'
import { createRisunestPresetAccess } from './risunestPresetAccess'
import { createRisunestSettingsAccess } from './risunestSettingsAccess'

// Wires the data, Lua check and floating window risunest* methods for one plugin frame.

export function createRisunestHostMethods(context: RisunestHostContext & { iframe: HTMLIFrameElement }) {
    const settings = createRisunestSettingsAccess(context)
    const presets = createRisunestPresetAccess(context)
    const characters = createRisunestCharacterAccess(context)
    const globalData = createRisunestGlobalDataAccess(context)
    const floating = createFloatingPluginWindow({ iframe: context.iframe, lifetimeSignal: context.lifetimeSignal })
    return {
        floating,
        methods: {
            risunestListSettingKeys: () => settings.listSettingKeys(),
            risunestReadSettings: (keys?: unknown) => settings.readSettings(keys),
            risunestWriteSettings: (values?: unknown) => settings.writeSettings(values),
            risunestListPresets: () => presets.listPresets(),
            risunestReadPreset: (target?: unknown, options?: unknown) => presets.readPreset(target, options),
            risunestUpdatePreset: (target?: unknown, set?: unknown) => presets.updatePreset(target, set),
            risunestCreatePreset: (input?: unknown) => presets.createPreset(input),
            risunestDeletePreset: (target?: unknown) => presets.deletePreset(target),
            risunestSelectPreset: (target?: unknown) => presets.selectPreset(target),
            risunestCreateCharacter: (input?: unknown) => characters.createCharacter(input),
            risunestTrashCharacter: (target?: unknown) => characters.trashCharacter(target),
            risunestRestoreCharacter: (target?: unknown) => characters.restoreCharacter(target),
            risunestReadGlobalVariables: (options?: unknown) => globalData.readGlobalVariables(options),
            risunestWriteGlobalVariables: (set?: unknown) => globalData.writeGlobalVariables(set),
            risunestListLoadouts: () => globalData.listLoadouts(),
            risunestWriteLoadout: (loadout?: unknown) => globalData.writeLoadout(loadout),
            risunestDeleteLoadout: (target?: unknown) => globalData.deleteLoadout(target),
            risunestCheckLuaSyntax: (source?: unknown) => checkRisunestLuaSyntax(source),
            risunestShowFloatingWindow: (options?: unknown, onChange?: unknown) => floating.show(options, onChange),
            risunestSetFloatingDragRegion: (region?: unknown) => floating.setDragRegion(region),
        },
    }
}
