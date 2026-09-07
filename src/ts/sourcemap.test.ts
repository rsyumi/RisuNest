import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const { originalPositionFor, destroy, MockSourceMapConsumer } = vi.hoisted(
    () => {
        const originalPositionFor = vi.fn()
        const destroy = vi.fn()
        const MockSourceMapConsumer = Object.assign(
            vi.fn(function MockSourceMapConsumer() {
                return {
                    originalPositionFor,
                    destroy,
                }
            }),
            {
                initialize: vi.fn(),
            },
        )

        return {
            originalPositionFor,
            destroy,
            MockSourceMapConsumer,
        }
    },
)

vi.mock('source-map', () => ({
    SourceMapConsumer: MockSourceMapConsumer,
}))

let translateStackTrace: typeof import('./sourcemap').translateStackTrace

describe('translateStackTrace', () => {
    const fetchMock = vi.fn()

    beforeEach(async () => {
        vi.resetModules()
        ;({ translateStackTrace } = await import('./sourcemap'))
        originalPositionFor.mockReset()
        destroy.mockReset()
        MockSourceMapConsumer.mockClear()
        MockSourceMapConsumer.initialize.mockClear()
        fetchMock.mockReset()
        vi.stubGlobal('fetch', fetchMock)
    })

    afterEach(() => {
        vi.unstubAllGlobals()
        vi.useRealTimers()
    })

    it('returns translated stack frames when sourcemaps resolve', async () => {
        fetchMock.mockResolvedValue({
            ok: true,
            json: async () => ({}),
        })
        originalPositionFor.mockReturnValue({
            source: 'src/lib/Others/AlertComp.svelte',
            line: 214,
            column: 15,
            name: 'loadTranslatedTrace',
        })

        const result = await translateStackTrace(`Error: boom
    at loadTranslatedTrace (http://localhost:4173/assets/index-abc123.js:10:15)`)

        expect(result).toEqual({
            stackTrace: `Error: boom
    at loadTranslatedTrace (src/lib/Others/AlertComp.svelte:214:15)`,
            didTranslate: true,
        })
        expect(fetchMock).toHaveBeenCalledWith(
            'http://localhost:4173/assets/index-abc123.js.map',
            expect.any(Object),
        )
        expect(destroy).not.toHaveBeenCalled()
        expect(originalPositionFor).toHaveBeenCalledWith({
            line: 10,
            column: 14,
        })
    })

    it('falls back to the original stack trace when sourcemap fetch fails', async () => {
        fetchMock.mockResolvedValue({
            ok: false,
            status: 404,
            statusText: 'Not Found',
        })

        const stackTrace = `Error: boom
    at loadTranslatedTrace (http://localhost:4173/assets/index-abc123.js:10:15)`
        const result = await translateStackTrace(stackTrace)

        expect(result).toEqual({
            stackTrace,
            didTranslate: false,
        })
    })

    it('coalesces concurrent loads and reuses a successful map', async () => {
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) })
        originalPositionFor.mockReturnValue({
            source: 'fixture.ts',
            line: 1,
            column: 0,
        })
        const stack =
            'Error: fixture\n at work (http://localhost/fixture.js:1:1)'
        await Promise.all([
            translateStackTrace(stack),
            translateStackTrace(stack),
        ])
        await translateStackTrace(stack)
        expect(fetchMock).toHaveBeenCalledTimes(1)
        expect(MockSourceMapConsumer).toHaveBeenCalledTimes(1)
    })

    it('backs off failed loads and retries after the failure TTL', async () => {
        vi.useFakeTimers()
        fetchMock.mockResolvedValue({ ok: false, status: 404 })
        const stack =
            'Error: fixture\n at work (http://localhost/missing.js:1:1)'
        await Promise.all([
            translateStackTrace(stack),
            translateStackTrace(stack),
        ])
        await translateStackTrace(stack)
        expect(fetchMock).toHaveBeenCalledTimes(1)
        await vi.advanceTimersByTimeAsync(60_001)
        await translateStackTrace(stack)
        expect(fetchMock).toHaveBeenCalledTimes(2)
    })

    it('bounds retained consumers and frees evicted maps', async () => {
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) })
        originalPositionFor.mockReturnValue({
            source: 'fixture.ts',
            line: 1,
            column: 0,
        })
        for (let i = 0; i < 12; i++) {
            await translateStackTrace(
                `Error: fixture\n at work (http://localhost/fixture-${i}.js:1:1)`,
            )
        }
        expect(destroy).toHaveBeenCalledTimes(8)
    })

    it('falls back immediately when the stack trace has no sourcemap-backed frames', async () => {
        const stackTrace = `Error: boom
    at loadTranslatedTrace (webpack-internal://src/lib/Others/AlertComp.svelte:10:15)`
        const result = await translateStackTrace(stackTrace)

        expect(fetchMock).not.toHaveBeenCalled()
        expect(result).toEqual({
            stackTrace,
            didTranslate: false,
        })
    })
})
