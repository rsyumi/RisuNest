export interface PresetChainState {
    presetChain?: string
    botPresets: Array<{ name?: string }>
}

export async function activatePresetChain(
    database: PresetChainState,
    changePreset: (index: number, saveCurrent: boolean) => Promise<void>,
    random: () => number,
    onMissing: (name: string) => void,
): Promise<void> {
    if (!database.presetChain) return
    const names = database.presetChain.split(',').map((name) => name.trim())
    const selectedName = names[Math.floor(random() * names.length)]
    const index = database.botPresets.findIndex((preset) => preset.name === selectedName)
    if (index < 0) {
        onMissing(selectedName)
        return
    }
    await changePreset(index, true)
}

export async function activatePresetChainForRequest(
    database: PresetChainState,
    changePreset: (index: number, saveCurrent: boolean) => Promise<void>,
    random: () => number,
    onMissing: (name: string) => void,
): Promise<void> {
    await activatePresetChain(database, changePreset, random, onMissing)
}
