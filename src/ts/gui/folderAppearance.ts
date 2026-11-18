import type { folder } from '../storage/database.svelte'

/** Folder appearance: how a stored folder looks in the sidebar and how the folder manager edits it. */

export const folderPresetColors = ['default', 'red', 'yellow', 'green', 'blue', 'indigo', 'purple', 'pink'] as const
export type FolderPresetColor = typeof folderPresetColors[number]
export type FolderIconType = 'text' | 'image' | 'glyph'
export type FolderColorMode = 'preset' | 'solid' | 'gradient'
/** Gradient directions in the order the manager offers them: → ↘ ↓ ↗. */
export const folderColorAngles = [90, 135, 180, 45] as const
const defaultAngle = 135

/** sRGB values of the Tailwind 700 shades behind the preset classes. */
const presetHex: Record<Exclude<FolderPresetColor, 'default'>, string> = {
    red: '#c10007', yellow: '#a65f00', green: '#008236', blue: '#1447e6',
    indigo: '#432dd7', purple: '#8200db', pink: '#c6005c',
}
const lightText = '#ffffff'
const darkText = '#111827'
/** Below this contrast the automatic colour gets a shadow and a chosen colour gets a warning. */
export const minimumTextContrast = 3

export function normalizeHexColor(value: unknown): string | null {
    if (typeof value !== 'string') return null
    const match = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(value.trim())
    if (!match) return null
    const digits = match[1].length === 3 ? [...match[1]].map((digit) => digit + digit).join('') : match[1]
    return `#${digits.toLowerCase()}`
}

function rgb(hex: string): [number, number, number] {
    return [1, 3, 5].map((index) => parseInt(hex.slice(index, index + 2), 16)) as [number, number, number]
}

function toHex(values: number[]): string {
    return `#${values.map((value) => Math.round(Math.min(255, Math.max(0, value))).toString(16).padStart(2, '0')).join('')}`
}

function luminance(hex: string): number {
    const [r, g, b] = rgb(hex).map((value) => {
        const channel = value / 255
        return channel <= 0.03928 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4
    })
    return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

/** WCAG contrast ratio of two `#rrggbb` colours. */
export function contrastRatio(first: string, second: string): number {
    const [light, dark] = [luminance(first), luminance(second)].sort((a, b) => b - a)
    return (light + 0.05) / (dark + 0.05)
}

/** `foreground` at `alpha` over an opaque `background`, blended the way the browser paints it. */
export function compositeOver(foreground: string, alpha: number, background: string): string {
    const top = rgb(foreground)
    const bottom = rgb(background)
    return toHex(top.map((value, index) => value * alpha + bottom[index] * (1 - alpha)))
}

export function folderPresetName(color: unknown): FolderPresetColor {
    return folderPresetColors.includes(color as FolderPresetColor) ? color as FolderPresetColor : 'default'
}

/** Opaque base colour of a preset, used as the first custom colour when switching away from it. */
export function folderPresetBaseHex(preset: FolderPresetColor, theme: { darkbg: string }): string {
    return preset === 'default' ? normalizeHexColor(theme.darkbg) ?? '#21222c' : presetHex[preset]
}

/** The preset fill as painted: the 700 shade (or the theme's dark background) at 50% over the sidebar. */
export function folderPresetFillHex(preset: FolderPresetColor, theme: { bgcolor: string, darkbg: string }): string | null {
    const background = normalizeHexColor(theme.bgcolor)
    if (!background) return null
    const top = preset === 'default' ? normalizeHexColor(theme.darkbg) : presetHex[preset]
    return top ? compositeOver(top, 0.5, background) : null
}

/** White or near-black, whichever reads better against every stop; a shadow when neither reaches the minimum. */
export function autoFolderTextColor(stops: string[]): { color: string, shadow: string | null, contrast: number } {
    const lowest = (text: string) => Math.min(...stops.map((stop) => contrastRatio(text, stop)))
    const light = lowest(lightText)
    const dark = lowest(darkText)
    const color = light >= dark ? lightText : darkText
    const contrast = Math.max(light, dark)
    return { color, shadow: contrast < minimumTextContrast ? (color === lightText ? darkText : lightText) : null, contrast }
}

function hueAndSaturation(hex: string): { hue: number, saturation: number, lightness: number } {
    const [r, g, b] = rgb(hex).map((value) => value / 255)
    const max = Math.max(r, g, b)
    const min = Math.min(r, g, b)
    const delta = max - min
    const lightness = (max + min) / 2
    if (delta === 0) return { hue: 0, saturation: 0, lightness }
    const saturation = delta / (1 - Math.abs(2 * lightness - 1))
    const sector = max === r ? ((g - b) / delta) % 6 : max === g ? (b - r) / delta + 2 : (r - g) / delta + 4
    return { hue: (sector * 60 + 360) % 360, saturation, lightness }
}

/** The preset closest in hue, kept in `color` so older readers show something similar; grey-ish colours map to `default`. */
export function nearestFolderPreset(stops: string[]): FolderPresetColor {
    const values = stops.map(rgb)
    const average = toHex([0, 1, 2].map((channel) => values.reduce((sum, value) => sum + value[channel], 0) / values.length))
    const { hue, saturation, lightness } = hueAndSaturation(average)
    if (saturation < 0.2 || lightness < 0.08 || lightness > 0.94) return 'default'
    let best: FolderPresetColor = 'default'
    let bestDistance = Infinity
    for (const [name, hex] of Object.entries(presetHex)) {
        const raw = Math.abs(hueAndSaturation(hex).hue - hue)
        const distance = Math.min(raw, 360 - raw)
        if (distance < bestDistance) {
            bestDistance = distance
            best = name as FolderPresetColor
        }
    }
    return best
}

/** What a stored folder shows, after validating every field; invalid or missing values fall back as older builds do. */
export interface FolderAppearance {
    iconType: FolderIconType
    /** The icon text, or the name when none is set. */
    text: string
    preset: FolderPresetColor
    /** One opaque colour or two gradient stops; null for a preset. */
    stops: string[] | null
    angle: number
    /** A chosen text colour; null for automatic. */
    textColor: string | null
}

type FolderFields = Pick<folder, 'name' | 'color' | 'imgFile' | 'iconText' | 'iconType' | 'colorStops' | 'colorAngle' | 'textColor'>

function storedStops(value: unknown): string[] | null {
    if (!Array.isArray(value) || value.length < 1 || value.length > 2) return null
    const stops = value.map(normalizeHexColor)
    return stops.every((stop): stop is string => stop !== null) ? stops : null
}

function storedAngle(value: unknown): number {
    return folderColorAngles.includes(value as typeof folderColorAngles[number]) ? value as number : defaultAngle
}

export function folderIconType(folder: Pick<folder, 'imgFile' | 'iconType'>): FolderIconType {
    const chosen = folder.iconType
    if (chosen === 'glyph' || chosen === 'text') return chosen
    if (chosen === 'image') return folder.imgFile ? 'image' : 'text'
    return folder.imgFile ? 'image' : 'text'
}

export function resolveFolderAppearance(folder: FolderFields): FolderAppearance {
    const iconText = typeof folder.iconText === 'string' && folder.iconText.trim() ? folder.iconText : ''
    return {
        iconType: folderIconType(folder),
        text: iconText || (typeof folder.name === 'string' ? folder.name : ''),
        preset: folderPresetName(folder.color),
        stops: storedStops(folder.colorStops),
        angle: storedAngle(folder.colorAngle),
        textColor: normalizeHexColor(folder.textColor),
    }
}

/** Background image for a custom colour; presets keep their classes. */
export function folderFillImage(appearance: Pick<FolderAppearance, 'stops' | 'angle'>): string | undefined {
    const stops = appearance.stops
    if (!stops) return undefined
    return stops.length === 1 ? `linear-gradient(${stops[0]}, ${stops[0]})` : `linear-gradient(${appearance.angle}deg, ${stops[0]}, ${stops[1]})`
}

/** The open folder's frame: the folder colour at 20%. */
export function folderFrameImage(appearance: Pick<FolderAppearance, 'stops' | 'angle'>): string | undefined {
    const stops = appearance.stops?.map((stop) => `${stop}33`)
    if (!stops) return undefined
    return stops.length === 1 ? `linear-gradient(${stops[0]}, ${stops[0]})` : `linear-gradient(${appearance.angle}deg, ${stops[0]}, ${stops[1]})`
}

/** Text colour and shadow on the tile; a null colour inherits the theme text colour (presets, automatic). */
export function folderTextStyle(appearance: Pick<FolderAppearance, 'stops' | 'textColor'>): { color: string | null, shadow: string | null } {
    if (appearance.textColor) return { color: appearance.textColor, shadow: null }
    if (!appearance.stops) return { color: null, shadow: null }
    const automatic = autoFolderTextColor(appearance.stops)
    return { color: automatic.color, shadow: automatic.shadow }
}

/** Contrast of a chosen text colour against what it sits on, or null when it cannot be known. */
export function chosenTextContrast(appearance: Pick<FolderAppearance, 'stops' | 'preset' | 'textColor'>, theme: { bgcolor: string, darkbg: string }): number | null {
    if (!appearance.textColor) return null
    if (appearance.stops) return Math.min(...appearance.stops.map((stop) => contrastRatio(appearance.textColor!, stop)))
    const fill = folderPresetFillHex(appearance.preset, theme)
    return fill ? contrastRatio(appearance.textColor, fill) : null
}

/** The manager's editable copy of a folder. Every mode keeps its values so switching back restores them. */
export interface FolderDraft {
    name: string
    iconText: string
    iconType: FolderIconType
    imgFile: string | null
    colorMode: FolderColorMode
    preset: FolderPresetColor
    solid: string
    gradient: [string, string]
    angle: number
    textMode: 'auto' | 'manual'
    textColor: string
}

export function folderDraftFrom(folder: FolderFields, theme: { darkbg: string }): FolderDraft {
    const appearance = resolveFolderAppearance(folder)
    const base = folderPresetBaseHex(appearance.preset, theme)
    const stops = appearance.stops
    return {
        name: typeof folder.name === 'string' ? folder.name : '',
        iconText: typeof folder.iconText === 'string' ? folder.iconText : '',
        iconType: appearance.iconType,
        imgFile: folder.imgFile || null,
        colorMode: !stops ? 'preset' : stops.length === 1 ? 'solid' : 'gradient',
        preset: appearance.preset,
        solid: stops?.[0] ?? base,
        gradient: stops?.length === 2 ? [stops[0], stops[1]] : [stops?.[0] ?? base, '#111827'],
        angle: appearance.angle,
        textMode: appearance.textColor ? 'manual' : 'auto',
        textColor: appearance.textColor ?? (stops ? autoFolderTextColor(stops).color : '#ffffff'),
    }
}

/**
 * The folder as the draft would store it. Unrelated keys are kept, appearance keys are written only when they
 * differ from what an absent key means, and `color` always stays a preset name for older readers.
 */
export function applyFolderDraft<T extends folder>(folder: T, draft: FolderDraft): T {
    const next: T = { ...folder, name: draft.name }
    const set = <K extends keyof folder>(key: K, value: folder[K] | undefined) => {
        if (value === undefined) delete next[key]
        else next[key] = value as T[K]
    }
    set('iconText', draft.iconText.trim() ? draft.iconText : undefined)
    if (draft.imgFile) next.imgFile = draft.imgFile
    else if (next.imgFile) {
        // A removed image is cleared the way upstream resets a folder image.
        next.imgFile = null
        next.img = ''
    }
    // 이미지 without an image shows the text, so it is stored as text.
    const chosen = draft.iconType === 'image' && !next.imgFile ? 'text' : draft.iconType
    set('iconType', chosen === folderIconType({ imgFile: next.imgFile }) ? undefined : chosen)
    const stops = draft.colorMode === 'solid' ? [normalizeHexColor(draft.solid)] : draft.colorMode === 'gradient' ? draft.gradient.map(normalizeHexColor) : null
    if (stops && stops.every((stop): stop is string => stop !== null)) {
        set('colorStops', stops)
        set('colorAngle', stops.length === 2 ? storedAngle(draft.angle) : undefined)
        next.color = nearestFolderPreset(stops)
    } else {
        set('colorStops', undefined)
        set('colorAngle', undefined)
        next.color = draft.preset
    }
    set('textColor', draft.textMode === 'manual' ? normalizeHexColor(draft.textColor) ?? undefined : undefined)
    return next
}

/** The order with the folder replaced by its characters, in folder order, at the folder's position. */
export function releaseFolder(order: (string | folder)[], folderId: string): (string | folder)[] {
    const index = order.findIndex((entry) => typeof entry !== 'string' && entry.id === folderId)
    if (index === -1) return order
    const entry = order[index] as folder
    return [...order.slice(0, index), ...entry.data, ...order.slice(index + 1)]
}
