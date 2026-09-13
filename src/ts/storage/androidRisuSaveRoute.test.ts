import { describe, expect, it, vi } from 'vitest'

import {
    createAndroidRisuSaveSpoolRoute,
    type AndroidRisuSaveSpoolRouteDependencies,
} from './androidRisuSaveRoute'

function dependencies(): AndroidRisuSaveSpoolRouteDependencies {
    return {
        confirmRestore: vi.fn(async () => true),
        discard: vi.fn(),
        restore: vi.fn(async () => undefined),
        unsupported: vi.fn(),
        failed: vi.fn(),
        onError: vi.fn(),
    }
}

describe('Android RisuSave spool route', () => {
    it('serializes multiple native restores and never forwards source bytes', async () => {
        const deps = dependencies()
        let releaseFirst!: () => void
        const firstPending = new Promise<void>((resolve) => {
            releaseFirst = resolve
        })
        vi.mocked(deps.restore)
            .mockImplementationOnce(() => firstPending)
            .mockResolvedValueOnce(undefined)
        const route = createAndroidRisuSaveSpoolRoute(deps)

        const pending = route.enqueue({
            requestId: 'request-1',
            ready: [
                {
                    token: '11111111-1111-4111-8111-111111111111',
                    displayName: 'first.RISUDAT',
                    bytes: 10_000,
                    totalBytes: 10_000,
                },
                {
                    token: '22222222-2222-4222-8222-222222222222',
                    displayName: 'second.risudat',
                    bytes: 20_000,
                    totalBytes: null,
                },
            ],
            failures: [],
        })
        await Promise.resolve()
        await Promise.resolve()

        expect(deps.restore).toHaveBeenCalledTimes(1)
        expect(deps.restore).toHaveBeenNthCalledWith(1, {
            source: {
                type: 'androidSpool',
                token: '11111111-1111-4111-8111-111111111111',
            },
            displayName: 'first.RISUDAT',
        })
        releaseFirst()
        await pending

        expect(deps.restore).toHaveBeenNthCalledWith(2, {
            source: {
                type: 'androidSpool',
                token: '22222222-2222-4222-8222-222222222222',
            },
            displayName: 'second.risudat',
        })
    })

    it('deduplicates a replayed token and discards a rejected restore', async () => {
        const deps = dependencies()
        vi.mocked(deps.confirmRestore).mockResolvedValueOnce(false)
        const route = createAndroidRisuSaveSpoolRoute(deps)
        const source = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'backup.risudat',
            bytes: 10,
            totalBytes: 10,
        }

        await route.enqueue({ requestId: 'request-1', ready: [source], failures: [] })
        await route.enqueue({ requestId: 'request-2', ready: [source], failures: [] })

        expect(deps.confirmRestore).toHaveBeenCalledOnce()
        expect(deps.restore).not.toHaveBeenCalled()
        expect(deps.discard).toHaveBeenCalledOnce()
        expect(deps.discard).toHaveBeenCalledWith(source)
    })

    it.each(['complete.RISUNEST', 'compatible.BIN'])(
        'passes %s to the native restore selector',
        async (displayName) => {
            const deps = dependencies()
            const route = createAndroidRisuSaveSpoolRoute(deps)

            await route.enqueue({
                requestId: 'request-lossless',
                ready: [
                    {
                        token: '44444444-4444-4444-8444-444444444444',
                        displayName,
                        bytes: 4096,
                    },
                ],
                failures: [],
            })

            expect(deps.restore).toHaveBeenCalledExactlyOnceWith({
                source: {
                    type: 'androidSpool',
                    token: '44444444-4444-4444-8444-444444444444',
                },
                displayName,
            })
        },
    )

    it('reports unsupported sources, spool failures, and restore errors without stopping the queue', async () => {
        const deps = dependencies()
        vi.mocked(deps.restore)
            .mockRejectedValueOnce(new Error('corrupt input'))
            .mockResolvedValueOnce(undefined)
        const route = createAndroidRisuSaveSpoolRoute(deps)

        await route.enqueue({
            requestId: 'request-1',
            ready: [
                {
                    token: '11111111-1111-4111-8111-111111111111',
                    displayName: 'card.charx',
                    bytes: 5,
                },
                {
                    token: '22222222-2222-4222-8222-222222222222',
                    displayName: 'broken.risudat',
                    bytes: 10,
                },
                {
                    token: '33333333-3333-4333-8333-333333333333',
                    displayName: 'valid.risudat',
                    bytes: 20,
                },
            ],
            failures: [{ displayName: 'missing.risudat', code: 'source-open-failed' }],
        })

        expect(deps.failed).toHaveBeenCalledWith({
            displayName: 'missing.risudat',
            code: 'source-open-failed',
        })
        expect(deps.unsupported).toHaveBeenCalledWith(expect.objectContaining({
            displayName: 'card.charx',
        }))
        expect(deps.discard).toHaveBeenCalledWith(expect.objectContaining({
            displayName: 'card.charx',
        }))
        expect(deps.onError).toHaveBeenCalledWith(
            expect.objectContaining({ displayName: 'broken.risudat' }),
            expect.objectContaining({ message: 'corrupt input' }),
        )
        expect(deps.restore).toHaveBeenLastCalledWith({
            source: {
                type: 'androidSpool',
                token: '33333333-3333-4333-8333-333333333333',
            },
            displayName: 'valid.risudat',
        })
    })
})
