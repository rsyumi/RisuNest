import { afterEach, describe, expect, it } from 'vitest'

import { createFloatingPluginWindow } from './floatingPluginWindow'

const lifetimes: AbortController[] = []
afterEach(() => {
    for (const lifetime of lifetimes.splice(0)) lifetime.abort()
    document.body.replaceChildren()
})

function pluginFrame() {
    const iframe = document.createElement('iframe')
    iframe.style.display = 'none'
    document.body.append(iframe)
    const lifetime = new AbortController()
    lifetimes.push(lifetime)
    return { lifetime, floating: createFloatingPluginWindow({ iframe, lifetimeSignal: lifetime.signal }) }
}

const bounds = { x: 10, y: 20, width: 300, height: 200 }

describe('floating plugin window', () => {
    it('rejects invalid arguments and leaves the frame and the document as they were', async () => {
        const { floating } = pluginFrame()
        const hidden = document.body.innerHTML
        const invalidShows: [unknown, unknown][] = [
            [undefined, () => {}], [{ bounds: { ...bounds, x: NaN } }, () => {}], [{ minSize: { width: -1, height: 10 } }, () => {}],
            [{ bounds }, undefined], [{ bounds }, 'callback'],
        ]
        for (const [options, onChange] of invalidShows) {
            await expect(floating.show(options, onChange)).rejects.toThrow(TypeError)
        }
        await expect(floating.setDragRegion(undefined)).rejects.toThrow(TypeError)
        await expect(floating.setDragRegion({ x: 0, y: 0, width: -5, height: 10 })).rejects.toThrow(TypeError)
        expect(document.body.innerHTML).toBe(hidden)

        await floating.show({ bounds }, () => {})
        const floatingDocument = document.body.innerHTML
        for (const [options, onChange] of invalidShows) {
            await expect(floating.show(options, onChange)).rejects.toThrow(TypeError)
        }
        expect(document.body.innerHTML).toBe(floatingDocument)
    })

    it('removes everything it added when the plugin unloads and refuses to float afterwards', async () => {
        const { floating, lifetime } = pluginFrame()
        const hidden = document.body.innerHTML
        await floating.setDragRegion({ x: 0, y: 0, width: 100, height: 30 })
        await floating.show({ bounds }, () => {})
        expect(document.body.innerHTML).not.toBe(hidden)
        lifetime.abort()
        expect(document.body.innerHTML).toBe(hidden)
        await expect(floating.show({ bounds }, () => {})).rejects.toThrow()
        expect(document.body.innerHTML).toBe(hidden)
    })
})
