/**
 * The woven-nest backdrop behind the onboarding wordmark. It is drawn once per
 * size from a fixed seed, so the same panel always produces the same strands
 * and nothing animates behind the text.
 */

type Rgb = readonly [number, number, number]

/** The logo gradient. These three are onboarding-only, not theme tokens. */
const TEAL: Rgb = [34, 200, 198]
const BLUE: Rgb = [59, 130, 246]
const INDIGO: Rgb = [110, 124, 248]

const STRAND_COUNT = 72
const SEED = 7

function mix(from: Rgb, to: Rgb, ratio: number): Rgb {
    return [
        Math.round(from[0] + (to[0] - from[0]) * ratio),
        Math.round(from[1] + (to[1] - from[1]) * ratio),
        Math.round(from[2] + (to[2] - from[2]) * ratio),
    ]
}

/** Park-Miller, so the pattern is identical on every device and every redraw. */
function seededRandom(seed: number): () => number {
    let state = seed
    return () => {
        state = (state * 16807) % 2147483647
        return state / 2147483647
    }
}

export function drawOnboardingWeave(canvas: HTMLCanvasElement): void {
    const width = canvas.clientWidth
    const height = canvas.clientHeight
    if (!width || !height) return
    const context = canvas.getContext('2d')
    if (!context) return

    const ratio = Math.min(2, window.devicePixelRatio || 1)
    canvas.width = Math.round(width * ratio)
    canvas.height = Math.round(height * ratio)
    context.setTransform(ratio, 0, 0, ratio, 0, 0)
    context.clearRect(0, 0, width, height)

    const base = context.createLinearGradient(0, 0, width, height)
    base.addColorStop(0, '#1b2a44')
    base.addColorStop(1, '#232338')
    context.fillStyle = base
    context.fillRect(0, 0, width, height)

    const centerX = width * 0.5
    const centerY = height * 0.66
    const glow = context.createRadialGradient(
        centerX, centerY, 4,
        centerX, centerY, Math.max(width, height) * 0.62,
    )
    glow.addColorStop(0, 'rgba(34,200,198,.34)')
    glow.addColorStop(0.45, 'rgba(59,130,246,.14)')
    glow.addColorStop(1, 'rgba(0,0,0,0)')
    context.fillStyle = glow
    context.fillRect(0, 0, width, height)

    const random = seededRandom(SEED)
    const reach = Math.max(width, height) * 0.48
    for (let index = 0; index < STRAND_COUNT; index += 1) {
        const tone = random()
        const radiusX = reach * (0.3 + 0.8 * random())
        const radiusY = radiusX * (0.34 + 0.14 * random())
        const start = random() * Math.PI * 2
        const sweep = 0.7 + random() * 1.9
        const rotation = (random() - 0.5) * 0.6
        const color = tone < 0.5 ? mix(TEAL, BLUE, tone * 2) : mix(TEAL, INDIGO, (tone - 0.5) * 2)
        const alpha = 0.05 + random() * 0.17
        context.beginPath()
        context.ellipse(centerX, centerY, radiusX, radiusY, rotation, start, start + sweep)
        context.strokeStyle = `rgba(${color[0]},${color[1]},${color[2]},${alpha.toFixed(3)})`
        context.lineWidth = 0.8 + random() * 1.7
        context.stroke()
    }
}

/** Draws now and after every resize. The returned function stops watching. */
export function observeOnboardingWeave(canvas: HTMLCanvasElement): () => void {
    drawOnboardingWeave(canvas)
    const target = canvas.parentElement
    if (!target || typeof ResizeObserver === 'undefined') return () => {}
    const observer = new ResizeObserver(() => drawOnboardingWeave(canvas))
    observer.observe(target)
    return () => observer.disconnect()
}
