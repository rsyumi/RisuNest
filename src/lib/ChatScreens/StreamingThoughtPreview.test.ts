// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, settled as domSettled, unmount } from 'svelte'
import { getStreamingThoughtPreview } from '../../ts/parser/streamingThoughtPreview'
import {
    asciiControlText,
    mixedGraphemeText,
    repeatedKoreanEmojiThought,
    wrapThought,
} from '../../ts/parser/largeThoughtFixtures.testUtils'
import StreamingThoughtPreviewHarness from './StreamingThoughtPreviewHarness.test.svelte'

type Harness = { setSource(value: string): void }

describe('expanded streaming thought', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined
    let frames: Map<number, FrameRequestCallback>
    let nextFrame: number
    let cancelFrame: ReturnType<typeof vi.fn>

    function runFrame() {
        const [id, callback] = frames.entries().next().value ?? []
        if (id === undefined) return false
        frames.delete(id)
        callback!(performance.now())
        return true
    }

    async function runFrames() {
        let count = 0
        while (runFrame()) {
            if (++count > 10_000) throw new Error('frames did not settle')
        }
        await domSettled()
        return count
    }

    function body() {
        return target.querySelector<HTMLElement>('[data-incremental-plain-text]')
    }

    async function open() {
        const details = target.querySelector('details')!
        details.open = true
        details.dispatchEvent(new Event('toggle'))
        await domSettled()
        return details
    }

    beforeEach(() => {
        frames = new Map()
        nextFrame = 0
        cancelFrame = vi.fn((id: number) => frames.delete(id))
        vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
            frames.set(++nextFrame, callback)
            return nextFrame
        })
        vi.stubGlobal('cancelAnimationFrame', cancelFrame)
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.unstubAllGlobals()
    })

    function start(source: string) {
        mounted = mount(StreamingThoughtPreviewHarness, {
            target,
            props: { initialSource: source },
        })
        return mounted as unknown as Harness
    }

    test('mounts the reproduced Korean/emoji thought in bounded units until the end is reachable', async () => {
        const source = wrapThought(repeatedKoreanEmojiThought())
        expect(source.length).toBe(500_049)
        const full = getStreamingThoughtPreview(source, true)!.full!
        start(source)
        await domSettled()
        expect(body()).toBeNull()
        await open()
        const first = body()!.textContent!
        expect(first.length).toBeGreaterThan(0)
        expect(first.length).toBeLessThan(20_000)
        expect(full.startsWith(first)).toBe(true)
        expect(await runFrames()).toBeGreaterThan(1)
        expect(body()!.textContent).toBe(full)
        expect(body()!.textContent!.endsWith('LATEST-SYNTHETIC')).toBe(true)
        const units = body()!.children
        expect(units.length).toBeGreaterThanOrEqual(Math.ceil(full.length / 2048))
        for (const unit of units) expect(unit.textContent!.length).toBeLessThanOrEqual(2048)
    })

    test('mounts an ASCII control and unbroken grapheme text exactly', async () => {
        for (const text of [
            asciiControlText(120_000),
            mixedGraphemeText(120_000, { spaces: false }),
        ]) {
            const source = wrapThought(text)
            start(source)
            await domSettled()
            await open()
            await runFrames()
            expect(body()!.textContent).toBe(getStreamingThoughtPreview(source, true)!.full)
            await unmount(mounted!)
            mounted = undefined
        }
    })

    test('keeps a split closing tag out of the expanded text', async () => {
        const harness = start('<Thoughts>alpha</Thou')
        await domSettled()
        await open()
        expect(body()!.textContent).toBe('alpha')
        harness.setSource('<Thoughts>alpha</Thoughts>middle<Thoughts>beta')
        await domSettled()
        await runFrames()
        expect(body()!.textContent).toBe('alpha\n\nbeta')
    })

    test('collapsing during incremental work stops further work', async () => {
        start(wrapThought(repeatedKoreanEmojiThought()))
        await domSettled()
        const details = await open()
        runFrame()
        await domSettled()
        expect(frames.size).toBe(1)
        details.open = false
        details.dispatchEvent(new Event('toggle'))
        await domSettled()
        expect(body()).toBeNull()
        expect(cancelFrame).toHaveBeenCalled()
        expect(frames.size).toBe(0)
    })

    test('a new streaming input continues or restarts the expansion', async () => {
        const thought = repeatedKoreanEmojiThought(20_000)
        const harness = start(`<Thoughts>${thought}`)
        await domSettled()
        await open()
        runFrame()
        await domSettled()
        const firstUnit = body()!.firstElementChild
        const appended = `<Thoughts>${thought}appended ${'\u{1f468}\u{200d}\u{1f469}'}</Thoughts>`
        harness.setSource(appended)
        await domSettled()
        await runFrames()
        expect(body()!.firstElementChild).toBe(firstUnit)
        expect(body()!.textContent).toBe(getStreamingThoughtPreview(appended, true)!.full)

        const replaced = `<Thoughts>REPLACED ${asciiControlText(60_000)}`
        harness.setSource(replaced)
        await domSettled()
        expect(body()!.textContent!.startsWith('REPLACED')).toBe(true)
        await runFrames()
        expect(body()!.textContent).toBe(getStreamingThoughtPreview(replaced, true)!.full)
    })
})
