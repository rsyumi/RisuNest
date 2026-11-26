import { getStartupExclusions } from '../storage/deviceSettings'
import { isStartupExcluded } from '../storage/recoveryMode.svelte'

const colorFields: ReadonlySet<string> = new Set(['colorScheme', 'colorSchemeName', 'customColorScheme'])
const sizeFields: ReadonlySet<string> = new Set(['sideBarSize', 'textAreaSize', 'textAreaTextSize'])
const textFields: ReadonlySet<string> = new Set(['textTheme', 'customTextTheme', 'customCSS', 'font', 'customFont'])

/** Whether this start leaves the text theme and custom CSS off. */
export function isThemeExcluded(): boolean {
    return isStartupExcluded('theme', getStartupExclusions())
}

/** Applies the display settings a received change set in the working set, the way startup applies them. */
export async function applyReceivedDisplaySettings(fields: ReadonlySet<string>): Promise<void> {
    const colors = [...fields].some((field) => colorFields.has(field))
    // The standard text colors follow the color scheme's light or dark type.
    const text = (colors || [...fields].some((field) => textFields.has(field))) && !isThemeExcluded()
    if (colors || text) {
        const { updateColorScheme, updateTextThemeAndCSS } = await import('./colorscheme')
        if (colors) updateColorScheme()
        if (text) updateTextThemeAndCSS()
    }
    if (fields.has('animationSpeed')) {
        const { updateAnimationSpeed } = await import('./animation')
        updateAnimationSpeed()
    }
    if (fields.has('heightMode')) {
        const { updateHeightMode } = await import('./heightMode')
        updateHeightMode()
    }
    if ([...fields].some((field) => sizeFields.has(field))) {
        const { updateGuisize } = await import('./guisize')
        updateGuisize()
    }
}
