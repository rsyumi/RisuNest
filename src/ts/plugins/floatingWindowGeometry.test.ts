import { describe, expect, it } from 'vitest'

import {
    clipRect, defaultBounds, fitBounds, floatingWindowLayers, moveBounds, parseDragRegion, parseFloatingOptions,
    resizeBounds, resizeEdges, type Rect,
} from './floatingWindowGeometry'

const viewport = { width: 1280, height: 720 }
const minSize = { width: 200, height: 150 }
const start: Rect = { x: 300, y: 200, width: 400, height: 300 }

const edgesOf = (rect: Rect) => ({ left: rect.x, top: rect.y, right: rect.x + rect.width, bottom: rect.y + rect.height })

describe('floating window options', () => {
    it('uses a minimum of 200 by 150, which also floors the minimum a plugin passes', () => {
        expect(parseFloatingOptions({})).toEqual({ bounds: undefined, minSize })
        expect(parseFloatingOptions({ minSize: { width: 50, height: 400.2 } }).minSize).toEqual({ width: 200, height: 401 })
        expect(parseFloatingOptions({ bounds: { x: -20, y: 9000, width: 0, height: 10 } }).bounds)
            .toEqual({ x: -20, y: 9000, width: 0, height: 10 })
    })

    it('rejects options that are not an object, non-finite numbers and negative sizes', () => {
        for (const options of [undefined, null, [], 5, 'bounds']) {
            expect(() => parseFloatingOptions(options), String(options)).toThrow(TypeError)
        }
        for (const bounds of [null, { x: 0, y: 0, width: 10 }, { x: NaN, y: 0, width: 10, height: 10 },
            { x: 0, y: Infinity, width: 10, height: 10 }, { x: 0, y: 0, width: '10', height: 10 },
            { x: 0, y: 0, width: -1, height: 10 }, { x: 0, y: 0, width: 10, height: -0.5 }]) {
            expect(() => parseFloatingOptions({ bounds }), JSON.stringify(bounds)).toThrow(TypeError)
        }
        for (const size of [{ width: -1, height: 10 }, { width: 10, height: NaN }, { width: 10 }, 7]) {
            expect(() => parseFloatingOptions({ minSize: size }), JSON.stringify(size)).toThrow(TypeError)
        }
    })

    it('accepts a drag region rectangle or null and rejects anything else', () => {
        expect(parseDragRegion(null)).toBeNull()
        expect(parseDragRegion({ x: -4, y: 0, width: 120, height: 32 })).toEqual({ x: -4, y: 0, width: 120, height: 32 })
        for (const region of [undefined, {}, { x: 0, y: 0, width: 10, height: -1 }, { x: 0, y: NaN, width: 10, height: 10 }]) {
            expect(() => parseDragRegion(region), JSON.stringify(region)).toThrow(TypeError)
        }
    })

    it('clips the drag region to the frame', () => {
        const frame = { width: 398, height: 298 }
        expect(clipRect({ x: 10, y: 0, width: 100, height: 30 }, frame)).toEqual({ x: 10, y: 0, width: 100, height: 30 })
        expect(clipRect({ x: -10, y: -5, width: 500, height: 30 }, frame)).toEqual({ x: 0, y: 0, width: 398, height: 25 })
        expect(clipRect({ x: 400, y: 0, width: 50, height: 30 }, frame)).toBeNull()
        expect(clipRect({ x: 0, y: 0, width: 0, height: 30 }, frame)).toBeNull()
    })
})

describe('floating window bounds', () => {
    it('keeps bounds inside the viewport and raises, cuts and moves the rest', () => {
        expect(fitBounds(start, minSize, viewport)).toEqual(start)
        expect(fitBounds({ x: 10.4, y: 20.6, width: 300.5, height: 200.2 }, minSize, viewport))
            .toEqual({ x: 10, y: 21, width: 301, height: 200 })
        expect(fitBounds({ x: -50, y: 900, width: 100, height: 50 }, minSize, viewport)).toEqual({ x: 0, y: 570, width: 200, height: 150 })
        expect(fitBounds({ x: 1200, y: -10, width: 400, height: 300 }, minSize, viewport)).toEqual({ x: 880, y: 0, width: 400, height: 300 })
        expect(fitBounds({ x: 10, y: 10, width: 2000, height: 1000 }, minSize, viewport)).toEqual({ x: 0, y: 0, width: 1280, height: 720 })
        expect(fitBounds(start, { width: 500, height: 350 }, viewport)).toEqual({ x: 300, y: 200, width: 500, height: 350 })
    })

    it('fills a dimension of a viewport smaller than the minimum', () => {
        expect(fitBounds(start, minSize, { width: 180, height: 600 })).toEqual({ x: 0, y: 200, width: 180, height: 300 })
        expect(fitBounds(start, minSize, { width: 900, height: 100 })).toEqual({ x: 300, y: 0, width: 400, height: 100 })
    })

    it('opens at 420 by 640, 24 px from the right edge and vertically centered', () => {
        expect(defaultBounds(minSize, viewport)).toEqual({ x: 836, y: 40, width: 420, height: 640 })
        expect(defaultBounds({ width: 500, height: 700 }, viewport)).toEqual({ x: 756, y: 10, width: 500, height: 700 })
        expect(defaultBounds(minSize, { width: 400, height: 500 })).toEqual({ x: 0, y: 0, width: 400, height: 500 })
    })

    it('moves by the pointer distance and stops at each viewport edge', () => {
        expect(moveBounds(start, -60, 80, minSize, viewport)).toEqual({ ...start, x: 240, y: 280 })
        expect(moveBounds(start, -1000, -1000, minSize, viewport)).toEqual({ ...start, x: 0, y: 0 })
        expect(moveBounds(start, 1000, 1000, minSize, viewport)).toEqual({ ...start, x: 880, y: 420 })
    })

    it('moves only the edges a resize zone names', () => {
        for (const edge of resizeEdges) {
            const before = edgesOf(start)
            const after = edgesOf(resizeBounds(start, edge, 37, 23, minSize, viewport))
            expect(after, edge).toEqual({
                left: before.left + (edge.includes('w') ? 37 : 0),
                top: before.top + (edge.includes('n') ? 23 : 0),
                right: before.right + (edge.includes('e') ? 37 : 0),
                bottom: before.bottom + (edge.includes('s') ? 23 : 0),
            })
        }
    })

    it('stops a resize at the viewport and at the minimum size while the opposite edges stay', () => {
        for (const edge of resizeEdges) {
            const before = edgesOf(start)
            const outward = edgesOf(resizeBounds(start, edge, edge.includes('w') ? -2000 : 2000, edge.includes('n') ? -2000 : 2000, minSize, viewport))
            expect(outward, `${edge} outward`).toEqual({
                left: edge.includes('w') ? 0 : before.left,
                top: edge.includes('n') ? 0 : before.top,
                right: edge.includes('e') ? viewport.width : before.right,
                bottom: edge.includes('s') ? viewport.height : before.bottom,
            })
            const inward = edgesOf(resizeBounds(start, edge, edge.includes('w') ? 2000 : -2000, edge.includes('n') ? 2000 : -2000, minSize, viewport))
            expect(inward, `${edge} inward`).toEqual({
                left: edge.includes('w') ? before.right - minSize.width : before.left,
                top: edge.includes('n') ? before.bottom - minSize.height : before.top,
                right: edge.includes('e') ? before.left + minSize.width : before.right,
                bottom: edge.includes('s') ? before.top + minSize.height : before.bottom,
            })
        }
    })

    it('keeps a full-viewport window filling a viewport smaller than the minimum while resizing', () => {
        const small = { width: 180, height: 120 }
        const filled = fitBounds(start, minSize, small)
        for (const edge of resizeEdges) {
            expect(resizeBounds(filled, edge, 25, -25, minSize, small), edge).toEqual({ x: 0, y: 0, width: 180, height: 120 })
        }
    })

    it('moves, then shrinks, a window a smaller viewport no longer holds', () => {
        const placed = { x: 800, y: 400, width: 400, height: 300 }
        expect(fitBounds(placed, minSize, { width: 1000, height: 600 })).toEqual({ x: 600, y: 300, width: 400, height: 300 })
        expect(fitBounds(placed, minSize, { width: 350, height: 250 })).toEqual({ x: 0, y: 0, width: 350, height: 250 })
    })
})

describe('floating window layers', () => {
    it('stacks each window, frame then handles, above the one below it', () => {
        const bottom = floatingWindowLayers(0)
        const top = floatingWindowLayers(1)
        expect(bottom.handles).toBeGreaterThan(bottom.frame)
        expect(top.frame).toBeGreaterThan(bottom.handles)
    })
})
