// Geometry of a floating plugin window in CSS pixels of the app window's client area, the space of
// position: fixed. Every result is whole pixels, inside the viewport and at least the minimum size.

export interface Rect { x: number; y: number; width: number; height: number }
export interface Size { width: number; height: number }
export type ResizeEdge = 'n' | 'e' | 's' | 'w' | 'ne' | 'se' | 'sw' | 'nw'

export const resizeEdges: readonly ResizeEdge[] = ['n', 'e', 's', 'w', 'ne', 'se', 'sw', 'nw']

const defaultSize: Size = { width: 420, height: 640 }
const defaultRightMargin = 24
const minimumSizeFloor: Size = { width: 200, height: 150 }

/** z-index of the bottom floating window, recorded in styles.css as z-floating-plugin-window. */
export const floatingWindowLayer = 500
const workDialogLayer = 1000

/** Layers of the window at `position` among the floating windows, counted from the bottom. */
export function floatingWindowLayers(position: number): { frame: number; handles: number } {
    const frame = Math.min(floatingWindowLayer + position * 2, workDialogLayer - 2)
    return { frame, handles: frame + 1 }
}

function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function readNumber(value: Record<string, unknown>, name: string, key: string): number {
    const number = value[key]
    if (typeof number !== 'number' || !Number.isFinite(number)) throw new TypeError(`${name}.${key} must be a finite number`)
    return number
}

function readSize(value: unknown, name: string): Size {
    if (!isRecord(value)) throw new TypeError(`${name} must be an object`)
    const size = { width: readNumber(value, name, 'width'), height: readNumber(value, name, 'height') }
    if (size.width < 0 || size.height < 0) throw new TypeError(`${name} must not have a negative size`)
    return size
}

function readRect(value: unknown, name: string): Rect {
    const size = readSize(value, name)
    const record = value as Record<string, unknown>
    return { x: readNumber(record, name, 'x'), y: readNumber(record, name, 'y'), ...size }
}

/** Validates risunestShowFloatingWindow options and resolves the minimum size. Throws on invalid input. */
export function parseFloatingOptions(options: unknown): { bounds?: Rect; minSize: Size } {
    if (!isRecord(options)) throw new TypeError('options must be an object')
    const bounds = options.bounds === undefined ? undefined : readRect(options.bounds, 'bounds')
    const minSize = options.minSize === undefined ? undefined : readSize(options.minSize, 'minSize')
    return {
        bounds,
        minSize: {
            width: Math.max(minimumSizeFloor.width, Math.ceil(minSize?.width ?? 0)),
            height: Math.max(minimumSizeFloor.height, Math.ceil(minSize?.height ?? 0)),
        },
    }
}

/** Validates a drag region: a rectangle relative to the frame, or null for none. Throws on invalid input. */
export function parseDragRegion(region: unknown): Rect | null {
    return region === null ? null : readRect(region, 'region')
}

const clamp = (value: number, low: number, high: number) => Math.min(Math.max(value, low), high)

/**
 * Raises each dimension to the minimum, cuts it to the viewport, and moves the window so all of it
 * is inside. A viewport smaller than the minimum is filled in that dimension.
 */
export function fitBounds(rect: Rect, minSize: Size, viewport: Size): Rect {
    const width = Math.min(Math.max(Math.round(rect.width), minSize.width), viewport.width)
    const height = Math.min(Math.max(Math.round(rect.height), minSize.height), viewport.height)
    return {
        x: clamp(Math.round(rect.x), 0, viewport.width - width),
        y: clamp(Math.round(rect.y), 0, viewport.height - height),
        width,
        height,
    }
}

/** 420 by 640, 24 px from the right edge and vertically centered, then fitted. */
export function defaultBounds(minSize: Size, viewport: Size): Rect {
    const width = Math.min(Math.max(defaultSize.width, minSize.width), viewport.width)
    const height = Math.min(Math.max(defaultSize.height, minSize.height), viewport.height)
    return fitBounds({
        x: viewport.width - defaultRightMargin - width,
        y: (viewport.height - height) / 2,
        width,
        height,
    }, minSize, viewport)
}

/** The window moved by the pointer's distance from where the move started. */
export function moveBounds(start: Rect, dx: number, dy: number, minSize: Size, viewport: Size): Rect {
    return fitBounds({ ...start, x: start.x + dx, y: start.y + dy }, minSize, viewport)
}

/** The window resized from one edge or corner; only the edges that zone names move. */
export function resizeBounds(start: Rect, edge: ResizeEdge, dx: number, dy: number, minSize: Size, viewport: Size): Rect {
    let left = start.x
    let top = start.y
    let right = start.x + start.width
    let bottom = start.y + start.height
    if (edge.includes('w')) left = Math.max(0, Math.min(Math.round(left + dx), right - minSize.width))
    if (edge.includes('e')) right = Math.min(viewport.width, Math.max(Math.round(right + dx), left + minSize.width))
    if (edge.includes('n')) top = Math.max(0, Math.min(Math.round(top + dy), bottom - minSize.height))
    if (edge.includes('s')) bottom = Math.min(viewport.height, Math.max(Math.round(bottom + dy), top + minSize.height))
    return fitBounds({ x: left, y: top, width: right - left, height: bottom - top }, minSize, viewport)
}

/** The part of `rect` inside a box of `size` at the origin, or null when nothing is left. */
export function clipRect(rect: Rect, size: Size): Rect | null {
    const x = Math.max(0, rect.x)
    const y = Math.max(0, rect.y)
    const right = Math.min(size.width, rect.x + rect.width)
    const bottom = Math.min(size.height, rect.y + rect.height)
    return right > x && bottom > y ? { x, y, width: right - x, height: bottom - y } : null
}

export const sameRect = (a: Rect, b: Rect) =>
    a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height
