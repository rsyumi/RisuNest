export interface PresetChainState {
    presetChain?: string
    botPresets: Array<{ id?: string; name?: string }>
}

export async function activatePresetChain(
    database: PresetChainState,
    setOverride: (id: string | null) => Promise<void>,
    random: () => number,
    onMissing: (name: string) => void,
): Promise<void> {
    if (!database.presetChain) {
        await setOverride(null)
        return
    }
    const names = database.presetChain.split(',').map((name) => name.trim())
    const selectedName = names[Math.floor(random() * names.length)]
    // An ID names the same preset when the library order changes before it is activated.
    const id = database.botPresets.find((preset) => preset.name === selectedName)?.id
    if (!id) {
        await setOverride(null)
        onMissing(selectedName)
        return
    }
    await setOverride(id)
}

/**
 * Applies the preset a request uses before it is built. `activateBinding` applies the target
 * conversation's binding and returns true when there is one, which replaces the chain.
 */
export async function activatePresetChainForRequest(
    database: () => PresetChainState,
    setOverride: (id: string | null) => Promise<void>,
    random: () => number,
    onMissing: (name: string) => void,
    activateBinding?: () => Promise<boolean>,
): Promise<void> {
    if (await activateBinding?.()) return
    // A library that replaced this one during the binding names its own chain.
    await activatePresetChain(database(), setOverride, random, onMissing)
}
