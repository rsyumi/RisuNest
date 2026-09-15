import { describe, expect, test, vi } from 'vitest'
import {
    inlayOptimizationGainRatio,
    inlayOptimizationWarnings,
    runInlayOptimization,
    selectInlayOptimizationTargets,
    type InlayOptimizationDeps,
    type InlayOptimizationProgress,
} from '../inlayOptimizationJob'
import { defaultInlayEncodeOptions, type InlayBlobMetadata, type InlayBlobType } from 'src/ts/storage/blobStore'

function asset(
    key: string,
    ext: string,
    size: number,
    extra: { inlayType?: InlayBlobType, width?: number, height?: number } = {},
): InlayBlobMetadata {
    return {
        key, kind: 'inlay', size, mime: '', name: `${key}.${ext}`, ext,
        inlayType: extra.inlayType ?? 'image', width: extra.width, height: extra.height,
    }
}

function deps(overrides: Partial<InlayOptimizationDeps> = {}): InlayOptimizationDeps & {
    written: [string, number][]
} {
    const written: [string, number][] = []
    return {
        written,
        read: async () => new Uint8Array(1000),
        encoder: { encodeNewInlayImage: async (key, _data, input) => ({
            data: new Uint8Array(400),
            metadata: { kind: 'inlay', inlayType: 'image', mime: 'image/webp', name: input.name, ext: 'webp', width: 8, height: 8 },
        }) },
        write: async (key, data) => { written.push([key, data.byteLength]) },
        ...overrides,
    }
}

const run = { options: defaultInlayEncodeOptions }

describe('selectInlayOptimizationTargets', () => {
    test('takes images that are not WebP yet', () => {
        const targets = selectInlayOptimizationTargets([
            asset('a', 'png', 10), asset('b', 'jpg', 10), asset('c', 'webp', 10), asset('d', '.WEBP', 10),
        ], { maxDimension: 0 })

        expect(targets.map((target) => target.key)).toEqual(['a', 'b'])
    })

    test('leaves audio, video, and signature entries alone', () => {
        const targets = selectInlayOptimizationTargets([
            asset('a', 'mp3', 10, { inlayType: 'audio' }),
            asset('b', 'mp4', 10, { inlayType: 'video' }),
            asset('c', 'json', 10, { inlayType: 'signature' }),
        ], { maxDimension: 0 })

        expect(targets).toEqual([])
    })

    test('takes WebP images only when they exceed the resolution limit', () => {
        const stored = [asset('small', 'webp', 10, { width: 800, height: 600 }), asset('large', 'webp', 10, { width: 4000, height: 600 })]

        expect(selectInlayOptimizationTargets(stored, { maxDimension: 0 }).map((target) => target.key)).toEqual([])
        expect(selectInlayOptimizationTargets(stored, { maxDimension: 1024 }).map((target) => target.key)).toEqual(['large'])
    })
})

describe('runInlayOptimization', () => {
    test('writes the new image and totals the saving', async () => {
        const dependencies = deps()

        const progress = await runInlayOptimization([asset('a', 'png', 1000), asset('b', 'png', 1000)], dependencies, run)

        expect(dependencies.written).toEqual([['a', 400], ['b', 400]])
        expect(progress).toEqual({ scanned: 2, total: 2, converted: 2, skipped: 0, failed: 0, beforeBytes: 2000, afterBytes: 800 })
    })

    test('always asks for WebP even when the stored format setting is something else', async () => {
        const encodeNewInlayImage = vi.fn(async (key: string, _data: Uint8Array, input: { name: string, options?: unknown }) => ({
            data: new Uint8Array(1), metadata: { kind: 'inlay' as const, inlayType: 'image' as const, mime: 'image/webp', name: input.name, ext: 'webp' },
        }))

        await runInlayOptimization([asset('a', 'png', 1000)], deps({ encoder: { encodeNewInlayImage } }), {
            options: { format: 'original', quality: 70, maxDimension: 2048, skipReencode: true, animationMaxFps: 0 },
        })

        expect(encodeNewInlayImage).toHaveBeenCalledWith('a', expect.any(Uint8Array), {
            name: 'a.png',
            options: { format: 'webp', quality: 70, maxDimension: 2048, skipReencode: false, animationMaxFps: 0 },
        })
    })

    test('keeps the original when the new image is not clearly smaller', async () => {
        const dependencies = deps({
            encoder: { encodeNewInlayImage: async (_key, _data, input) => ({
                data: new Uint8Array(Math.ceil(1000 * inlayOptimizationGainRatio) + 1),
                metadata: { kind: 'inlay', inlayType: 'image', mime: 'image/webp', name: input.name, ext: 'webp' },
            }) },
        })

        const progress = await runInlayOptimization([asset('a', 'png', 1000)], dependencies, run)

        expect(dependencies.written).toEqual([])
        expect(progress).toMatchObject({ converted: 0, skipped: 1, failed: 0, beforeBytes: 0, afterBytes: 0 })
    })

    test('skips entries it cannot read or re-encode without failing the run', async () => {
        const dependencies = deps({
            read: async (key) => key === 'missing' ? null : new Uint8Array(1000),
            encoder: { encodeNewInlayImage: async (key, _data, input) => {
                if (key === 'animated') throw new Error('cannot re-encode')
                return { data: new Uint8Array(10), metadata: { kind: 'inlay', inlayType: 'image', mime: 'image/webp', name: input.name, ext: 'webp' } }
            } },
        })

        const progress = await runInlayOptimization(
            [asset('missing', 'png', 1000), asset('animated', 'gif', 1000), asset('plain', 'png', 1000)],
            dependencies,
            run,
        )

        expect(dependencies.written).toEqual([['plain', 10]])
        expect(progress).toMatchObject({ scanned: 3, converted: 1, skipped: 2, failed: 0 })
    })

    test('counts a failed write and keeps going', async () => {
        const dependencies = deps({
            write: async (key) => { if (key === 'a') throw new Error('write failed') },
        })

        const progress = await runInlayOptimization([asset('a', 'png', 1000), asset('b', 'png', 1000)], dependencies, run)

        expect(progress).toMatchObject({ scanned: 2, converted: 1, skipped: 0, failed: 1 })
    })

    test('stops at the next entry boundary when cancelled and reports progress per entry', async () => {
        const seen: InlayOptimizationProgress[] = []
        let cancelled = false
        const dependencies = deps()

        const progress = await runInlayOptimization(
            [asset('a', 'png', 1000), asset('b', 'png', 1000), asset('c', 'png', 1000)],
            dependencies,
            {
                options: defaultInlayEncodeOptions,
                isCancelled: () => cancelled,
                onProgress: (value) => { seen.push(value); cancelled = true },
            },
        )

        expect(dependencies.written).toEqual([['a', 400]])
        expect(seen).toHaveLength(1)
        expect(progress).toMatchObject({ scanned: 1, total: 3, converted: 1 })
    })
})

describe('inlayOptimizationWarnings', () => {
    test('says nothing when the device stores WebP and syncs nowhere', () => {
        expect(inlayOptimizationWarnings({ storedFormat: 'webp', syncConfigured: false })).toEqual([])
    })

    test('warns that the run differs from a non-WebP storage setting', () => {
        expect(inlayOptimizationWarnings({ storedFormat: 'original', syncConfigured: false })).toEqual(['format'])
        expect(inlayOptimizationWarnings({ storedFormat: 'png', syncConfigured: false })).toEqual(['format'])
    })

    test('warns about the re-upload, and about the download only when assets live on the server', () => {
        expect(inlayOptimizationWarnings({ storedFormat: 'webp', syncConfigured: true, residencyPolicy: 'full' })).toEqual(['sync'])
        expect(inlayOptimizationWarnings({ storedFormat: 'webp', syncConfigured: true, residencyPolicy: 'remote' })).toEqual(['sync', 'remote'])
        expect(inlayOptimizationWarnings({ storedFormat: 'webp', syncConfigured: false, residencyPolicy: 'remote' })).toEqual([])
    })
})
