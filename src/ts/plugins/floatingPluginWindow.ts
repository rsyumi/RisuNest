import {
    clipRect, defaultBounds, fitBounds, floatingWindowLayers, moveBounds, parseDragRegion, parseFloatingOptions,
    resizeBounds, resizeEdges, sameRect, type Rect, type ResizeEdge, type Size,
} from './floatingWindowGeometry'

// The plugin frame as a floating window: risunestShowFloatingWindow, risunestSetFloatingDragRegion.
// The frame never changes parent, since that reloads it; floating changes only its inline style and
// adds one sibling element holding the resize zones and the drag region.

export interface FloatingPluginWindowHost {
    iframe: HTMLIFrameElement
    lifetimeSignal: AbortSignal
}

export interface FloatingWindowState {
    bounds: Rect
    viewport: Size
}

const frameBorderWidth = 1
const frameChrome = {
    'box-sizing': 'border-box',
    border: `${frameBorderWidth}px solid var(--risu-theme-darkborderc)`,
    'border-radius': '0.5rem',
    'box-shadow': '0 10px 15px -3px rgb(0 0 0 / 0.1), 0 4px 6px -4px rgb(0 0 0 / 0.1)',
    'background-color': 'var(--risu-theme-bgcolor)',
}

const zoneCursors: Record<ResizeEdge, string> = {
    n: 'ns-resize', s: 'ns-resize', e: 'ew-resize', w: 'ew-resize',
    ne: 'nesw-resize', sw: 'nesw-resize', nw: 'nwse-resize', se: 'nwse-resize',
}

// How far the zones reach outside the frame and into it, and how far a corner zone runs along each edge.
function zoneMetrics() {
    const coarse = typeof matchMedia === 'function' && matchMedia('(pointer: coarse)').matches
    return coarse ? { outside: 12, inside: 12, corner: 24 } : { outside: 6, inside: 4, corner: 10 }
}

function readViewport(): Size {
    return { width: document.documentElement.clientWidth, height: document.documentElement.clientHeight }
}

interface StackEntry { setLayers(position: number): void }

// Floating windows of every plugin, bottom to top.
const stack: StackEntry[] = []

function restack(): void {
    stack.forEach((entry, position) => entry.setLayers(position))
}

function raise(entry: StackEntry): void {
    const index = stack.indexOf(entry)
    if (index === stack.length - 1 && index >= 0) return
    if (index >= 0) stack.splice(index, 1)
    stack.push(entry)
    restack()
}

function lower(entry: StackEntry): void {
    const index = stack.indexOf(entry)
    if (index < 0) return
    stack.splice(index, 1)
    restack()
}

// One move or resize at a time across all windows. While it runs, every frame in the document
// ignores pointer input: a frame under the pointer would otherwise take the moves and the release
// even from an element that holds pointer capture.
interface SuspendedFrame { frame: HTMLIFrameElement; hadStyle: boolean; value: string; priority: string }
let suspendedFrames: SuspendedFrame[] | null = null

function suspendFramePointerInput(): void {
    suspendedFrames = [...document.querySelectorAll('iframe')].map(frame => ({
        frame,
        hadStyle: frame.hasAttribute('style'),
        value: frame.style.getPropertyValue('pointer-events'),
        priority: frame.style.getPropertyPriority('pointer-events'),
    }))
    for (const { frame } of suspendedFrames) frame.style.setProperty('pointer-events', 'none', 'important')
}

function restorePointerInput({ frame, hadStyle, value, priority }: SuspendedFrame): void {
    if (value) frame.style.setProperty('pointer-events', value, priority)
    else frame.style.removeProperty('pointer-events')
    if (!hadStyle && frame.style.length === 0) frame.removeAttribute('style')
}

function resumeFramePointerInput(): void {
    for (const suspended of suspendedFrames ?? []) restorePointerInput(suspended)
    suspendedFrames = null
}

/** The frame's style attribute as it is without a running move or resize. */
function frameStyleOutsideDrag(frame: HTMLIFrameElement): string | null {
    const suspended = suspendedFrames?.find(entry => entry.frame === frame)
    if (!suspended) return frame.getAttribute('style')
    restorePointerInput(suspended)
    const style = frame.getAttribute('style')
    frame.style.setProperty('pointer-events', 'none', 'important')
    return style
}

interface Drag {
    pointerId: number
    x: number
    y: number
    start: Rect
    element: HTMLElement
    listeners: AbortController
}

interface Session {
    bounds: Rect
    minSize: Size
    viewport: Size
    onChange: (state: FloatingWindowState) => unknown
    frameStyle: string | null
    container: HTMLDivElement
    region: HTMLDivElement
    zones: Map<ResizeEdge, HTMLDivElement>
    metrics: ReturnType<typeof zoneMetrics>
    entry: StackEntry
    listeners: AbortController
    drag?: Drag
    reportTimer?: ReturnType<typeof setTimeout>
}

export function createFloatingPluginWindow(host: FloatingPluginWindowHost) {
    const { iframe, lifetimeSignal } = host
    let dragRegion: Rect | null = null
    let session: Session | null = null

    const state = (current: Session): FloatingWindowState => ({ bounds: { ...current.bounds }, viewport: { ...current.viewport } })

    function placeZones(current: Session): void {
        const { outside, inside, corner } = current.metrics
        const thickness = `${outside + inside}px`
        const cornerSize = `${outside + corner}px`
        for (const [edge, zone] of current.zones) {
            const style = zone.style
            style.top = style.bottom = style.left = style.right = style.width = style.height = ''
            if (edge.length === 2) {
                style.width = style.height = cornerSize
                style[edge[0] === 'n' ? 'top' : 'bottom'] = '0'
                style[edge[1] === 'w' ? 'left' : 'right'] = '0'
            } else if (edge === 'n' || edge === 's') {
                style.left = style.right = cornerSize
                style.height = thickness
                style[edge === 'n' ? 'top' : 'bottom'] = '0'
            } else {
                style.top = style.bottom = cornerSize
                style.width = thickness
                style[edge === 'w' ? 'left' : 'right'] = '0'
            }
        }
    }

    function layout(current: Session): void {
        const { bounds } = current
        const { outside } = current.metrics
        iframe.style.left = `${bounds.x}px`
        iframe.style.top = `${bounds.y}px`
        iframe.style.width = `${bounds.width}px`
        iframe.style.height = `${bounds.height}px`
        const container = current.container.style
        container.left = `${bounds.x - outside}px`
        container.top = `${bounds.y - outside}px`
        container.width = `${bounds.width + outside * 2}px`
        container.height = `${bounds.height + outside * 2}px`
        const content = { width: bounds.width - frameBorderWidth * 2, height: bounds.height - frameBorderWidth * 2 }
        const region = dragRegion && clipRect(dragRegion, content)
        const style = current.region.style
        style.display = region ? '' : 'none'
        if (!region) return
        style.left = `${outside + frameBorderWidth + region.x}px`
        style.top = `${outside + frameBorderWidth + region.y}px`
        style.width = `${region.width}px`
        style.height = `${region.height}px`
    }

    function scheduleReport(current: Session): void {
        if (current.reportTimer !== undefined) return
        current.reportTimer = setTimeout(() => {
            current.reportTimer = undefined
            if (session !== current) return
            try {
                // The guest's answer does not matter; a failing callback must not reach the host.
                void Promise.resolve(current.onChange(state(current))).catch(() => {})
            } catch {}
        })
    }

    function endDrag(current: Session, report: boolean): void {
        const drag = current.drag
        if (!drag) return
        current.drag = undefined
        drag.listeners.abort()
        try {
            if (drag.element.hasPointerCapture(drag.pointerId)) drag.element.releasePointerCapture(drag.pointerId)
        } catch {}
        resumeFramePointerInput()
        if (report && !sameRect(drag.start, current.bounds)) scheduleReport(current)
    }

    function beginDrag(current: Session, event: PointerEvent, action: ResizeEdge | 'move', element: HTMLElement): void {
        if (session !== current || !event.isPrimary || event.button !== 0) return
        event.preventDefault()
        raise(current.entry)
        if (suspendedFrames) return
        const listeners = new AbortController()
        const drag: Drag = { pointerId: event.pointerId, x: event.clientX, y: event.clientY, start: { ...current.bounds }, element, listeners }
        current.drag = drag
        suspendFramePointerInput()
        try {
            element.setPointerCapture(event.pointerId)
        } catch {}
        const end = (event: Event) => {
            if (!(event instanceof PointerEvent) || event.pointerId === drag.pointerId) endDrag(current, true)
        }
        const move = (event: PointerEvent) => {
            if (event.pointerId !== drag.pointerId) return
            // The release can happen where the host never sees it.
            if (event.buttons === 0) return endDrag(current, true)
            const dx = event.clientX - drag.x
            const dy = event.clientY - drag.y
            current.bounds = action === 'move'
                ? moveBounds(drag.start, dx, dy, current.minSize, current.viewport)
                : resizeBounds(drag.start, action, dx, dy, current.minSize, current.viewport)
            layout(current)
        }
        const options = { capture: true, signal: listeners.signal }
        window.addEventListener('pointermove', move, options)
        window.addEventListener('pointerup', end, options)
        window.addEventListener('pointercancel', end, options)
        window.addEventListener('blur', event => { if (event.target === window) endDrag(current, true) }, { signal: listeners.signal })
        element.addEventListener('lostpointercapture', end, { signal: listeners.signal })
    }

    function begin(): Session {
        const frameStyle = frameStyleOutsideDrag(iframe)
        const container = document.createElement('div')
        container.style.cssText = 'position:fixed;pointer-events:none'
        const handle = () => {
            const element = document.createElement('div')
            element.style.cssText = 'position:absolute;pointer-events:auto;touch-action:none;user-select:none;-webkit-user-select:none'
            return element
        }
        const region = handle()
        const zones = new Map(resizeEdges.map(edge => [edge, handle()]))
        const listeners = new AbortController()
        const current: Session = {
            bounds: { x: 0, y: 0, width: 0, height: 0 },
            minSize: { width: 0, height: 0 },
            viewport: readViewport(),
            onChange: () => {},
            frameStyle,
            container,
            region,
            zones,
            metrics: zoneMetrics(),
            entry: {
                setLayers(position) {
                    const layers = floatingWindowLayers(position)
                    iframe.style.zIndex = String(layers.frame)
                    container.style.zIndex = String(layers.handles)
                },
            },
            listeners,
        }
        region.addEventListener('pointerdown', event => beginDrag(current, event, 'move', region))
        for (const [edge, zone] of zones) {
            zone.style.cursor = zoneCursors[edge]
            zone.addEventListener('pointerdown', event => beginDrag(current, event, edge, zone))
        }
        // Zones come after the region so an edge stays grabbable where the region reaches it.
        container.append(region, ...zones.values())
        placeZones(current)
        iframe.style.display = 'block'
        iframe.style.position = 'fixed'
        for (const [property, value] of Object.entries(frameChrome)) iframe.style.setProperty(property, value)
        iframe.after(container)
        window.addEventListener('resize', () => {
            const viewport = readViewport()
            if (viewport.width === current.viewport.width && viewport.height === current.viewport.height) return
            current.viewport = viewport
            current.metrics = zoneMetrics()
            placeZones(current)
            current.bounds = fitBounds(current.bounds, current.minSize, viewport)
            layout(current)
            scheduleReport(current)
        }, { signal: listeners.signal })
        return current
    }

    function stop(): void {
        const current = session
        if (!current) return
        session = null
        endDrag(current, false)
        clearTimeout(current.reportTimer)
        current.listeners.abort()
        current.container.remove()
        lower(current.entry)
        if (current.frameStyle === null) iframe.removeAttribute('style')
        else iframe.setAttribute('style', current.frameStyle)
        // Another window's move or resize may still be running.
        if (suspendedFrames?.some(entry => entry.frame === iframe)) iframe.style.setProperty('pointer-events', 'none', 'important')
    }

    lifetimeSignal.addEventListener('abort', stop, { once: true })

    return {
        async show(options?: unknown, onChange?: unknown): Promise<FloatingWindowState> {
            const parsed = parseFloatingOptions(options)
            if (typeof onChange !== 'function') throw new TypeError('onChange must be a function')
            if (lifetimeSignal.aborted) throw new Error('The plugin is unloading')
            const viewport = readViewport()
            const current = session ??= begin()
            endDrag(current, false)
            clearTimeout(current.reportTimer)
            current.reportTimer = undefined
            current.viewport = viewport
            current.minSize = parsed.minSize
            current.bounds = parsed.bounds ? fitBounds(parsed.bounds, parsed.minSize, viewport) : defaultBounds(parsed.minSize, viewport)
            current.onChange = onChange as Session['onChange']
            raise(current.entry)
            layout(current)
            return state(current)
        },
        async setDragRegion(region?: unknown): Promise<void> {
            dragRegion = parseDragRegion(region)
            if (session) layout(session)
        },
        /** Ends floating and puts back everything it changed. showContainer and hideContainer call it. */
        stop,
    }
}
