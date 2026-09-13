import { describe, expect, it, vi } from 'vitest'
import {
    NativeCommitTransport,
    isLargeCommit,
    LARGE_COMMIT_BYTES,
    type CommitEnvelope,
    type SharedWebview,
} from './nativeCommitTransport'

function fixture(large = true): CommitEnvelope {
    return {
        commit: {
            expectedRevision: 1,
            rootMutations: [
                {
                    type: 'set',
                    key: 'username',
                    value: large ? 'x'.repeat(LARGE_COMMIT_BYTES) : 'small',
                },
            ],
        },
        assetAliases: [],
    }
}
function harness(
    options: {
        windows?: boolean
        android?: boolean
        linux?: boolean
        macos?: boolean
        unsupported?: boolean
        fail?: string
        ack?: number
    } = {},
) {
    let listener: Parameters<SharedWebview['addEventListener']>[1] | undefined
    const webview: SharedWebview = {
        addEventListener: vi.fn((_, fn) => {
            listener = fn
        }),
        removeEventListener: vi.fn(),
        releaseBuffer: vi.fn(),
    }
    const bytes = new Uint8Array([1, 2, 3, 4, 5])
    const received: number[] = []
    const buffer = new ArrayBuffer(3)
    const invoke = vi.fn(async (command: string, args: any) => {
        if (options.fail === command) throw new Error('native failed')
        if (command === 'pds_commit_android_open')
            return { capacity: 32 * 1024 }
        if (command === 'pds_commit_android_chunk')
            return args.offset + new TextEncoder().encode(args.chunk).length
        if (command === 'pds_commit_shared_open') {
            if (options.unsupported) return null
            listener!({
                additionalData: {
                    kind: 'pds-commit',
                    requestId: args.requestId,
                    id: 'session',
                },
                getBuffer: () => buffer,
            })
            return { id: 'session', capacity: buffer.byteLength }
        }
        if (command === 'pds_commit_shared_chunk') {
            received.push(...new Uint8Array(buffer).subarray(0, args.length))
            return options.ack ?? args.offset + args.length
        }
        return { revision: 2 }
    })
    const encode = vi.fn(async () => bytes)
    const transport = new NativeCommitTransport({
        windows: () => options.windows ?? true,
        android: () => options.android ?? false,
        linux: () => options.linux ?? false,
        macos: () => options.macos ?? false,
        invoke,
        encode,
        shared: () => webview,
    })
    return { transport, invoke, encode, webview, received }
}

describe('native commit transport', () => {
    it.each(['linux', 'macos'] as const)(
        'encodes %s large saves and submits raw bytes without touching shared buffers',
        async (os) => {
            const h = harness({ windows: false, [os]: true })
            await expect(h.transport.commit(fixture())).resolves.toEqual({
                revision: 2,
            })
            expect(h.encode).toHaveBeenCalledExactlyOnceWith(fixture())
            expect(h.invoke).toHaveBeenCalledExactlyOnceWith(
                'pds_commit_raw',
                new Uint8Array([1, 2, 3, 4, 5]),
            )
            expect(h.webview.addEventListener).not.toHaveBeenCalled()
        },
    )

    it.each(['linux', 'macos'] as const)(
        'keeps small %s saves on JSON and only falls back before raw submission',
        async (os) => {
            const h = harness({ windows: false, [os]: true })
            await h.transport.commit(fixture(false))
            expect(h.encode).not.toHaveBeenCalled()
            expect(h.invoke).toHaveBeenCalledExactlyOnceWith(
                'pds_commit',
                fixture(false),
            )
            h.invoke.mockClear()
            h.encode.mockRejectedValueOnce(
                new DOMException('Cannot clone', 'DataCloneError'),
            )
            await h.transport.commit(fixture())
            expect(h.invoke).toHaveBeenCalledExactlyOnceWith(
                'pds_commit',
                fixture(),
            )
        },
    )

    it.each(['linux', 'macos'] as const)(
        'does not replay an ambiguous %s raw commit and allows the next queued save',
        async (os) => {
            const h = harness({
                windows: false,
                [os]: true,
                fail: 'pds_commit_raw',
            })
            await expect(h.transport.commit(fixture())).rejects.toThrow(
                'native failed',
            )
            expect(h.invoke).toHaveBeenCalledTimes(1)
            expect(h.invoke.mock.calls[0][0]).toBe('pds_commit_raw')
            await expect(h.transport.commit(fixture(false))).resolves.toEqual({
                revision: 2,
            })
        },
    )

    it.each(['linux', 'macos'] as const)(
        'does not turn the Windows shared-buffer budget into a %s save limit',
        async (os) => {
            const h = harness({ windows: false, [os]: true })
            const bytes = new Uint8Array(64 * 1024 * 1024 + 1)
            h.encode.mockResolvedValueOnce(bytes)
            await h.transport.commit(fixture())
            expect(h.invoke).toHaveBeenCalledTimes(1)
            expect(h.invoke.mock.calls[0][0]).toBe('pds_commit_raw')
            expect(h.invoke.mock.calls[0][1]).toBe(bytes)
            expect(h.webview.addEventListener).not.toHaveBeenCalled()
        },
    )

    it('falls back before submission for non-cloneable values, but propagates encoding failures', async () => {
        const h = harness()
        h.encode.mockRejectedValueOnce(new DOMException('Cannot clone', 'DataCloneError'))
        await h.transport.commit(fixture())
        expect(h.invoke).toHaveBeenCalledExactlyOnceWith('pds_commit', fixture())
        h.invoke.mockClear()
        h.encode.mockRejectedValueOnce(new Error('Invalid JSON value'))
        await expect(h.transport.commit(fixture())).rejects.toThrow('Invalid JSON value')
        expect(h.invoke).not.toHaveBeenCalled()
    })

    it('releases the native producer even when releasing the JS view fails', async () => {
        const h = harness()
        vi.mocked(h.webview.releaseBuffer).mockImplementation(() => {
            throw new Error('Detached')
        })
        await expect(h.transport.commit(fixture())).rejects.toThrow('Detached')
        expect(h.invoke).toHaveBeenLastCalledWith('pds_commit_shared_cancel', {
            id: 'session',
        })
        expect(
            h.invoke.mock.calls.filter(([command]) => command === 'pds_commit_shared_finish'),
        ).toHaveLength(1)
    })

    it('bounds the routing probe for both strings and many small fields', () => {
        expect(isLargeCommit(fixture(false))).toBe(false)
        expect(isLargeCommit(fixture())).toBe(true)
        expect(isLargeCommit(Array.from({ length: 10_000 }, () => 1))).toBe(true)
    })
    it('keeps other platforms and small requests on ordinary invoke without encoding', async () => {
        for (const [windows, input] of [
            [false, fixture()],
            [true, fixture(false)],
        ] as const) {
            const h = harness({ windows })
            await h.transport.commit(input)
            expect(h.invoke).toHaveBeenCalledExactlyOnceWith('pds_commit', input)
            expect(h.encode).not.toHaveBeenCalled()
        }
    })
    it('routes large Android commits through the Worker and strings, retaining small JSON saves', async () => {
        const h = harness({ windows: false, android: true })
        await h.transport.commit(fixture(false))
        expect(h.encode).not.toHaveBeenCalled()
        h.invoke.mockClear()
        h.encode.mockResolvedValueOnce(new TextEncoder().encode('{"한글":"🐿️"}'))
        await h.transport.commit(fixture())
        expect(h.invoke.mock.calls.map(([command]) => command)).toEqual([
            'pds_commit_android_open',
            'pds_commit_android_chunk',
            'pds_commit_android_finish',
            'pds_commit_android_cancel',
        ])
        expect(h.webview.addEventListener).not.toHaveBeenCalled()
    })
    it('retains JSON saves above the Android assembly limit, without raw number arrays', async () => {
        const h = harness({ windows: false, android: true })
        h.encode.mockResolvedValueOnce(new Uint8Array(64 * 1024 * 1024 + 1))
        await h.transport.commit(fixture())
        expect(h.invoke).toHaveBeenCalledExactlyOnceWith('pds_commit', fixture())
    })
    it('preserves Android clone fallback, errors and queue recovery', async () => {
        const h = harness({
            windows: false,
            android: true,
            fail: 'pds_commit_android_finish',
        })
        h.encode.mockRejectedValueOnce(new DOMException('Cannot clone', 'DataCloneError'))
        await h.transport.commit(fixture())
        h.encode.mockResolvedValueOnce(new TextEncoder().encode('{}'))
        await expect(h.transport.commit(fixture())).rejects.toThrow('native failed')
        await h.transport.commit(fixture(false))
        expect(h.invoke.mock.calls.filter(([cmd]) => cmd === 'pds_commit')).toHaveLength(2)
    })
    it('copies every byte once in ordered acknowledged chunks and releases both sides', async () => {
        const h = harness()
        expect(await h.transport.commit(fixture())).toEqual({ revision: 2 })
        expect(h.received).toEqual([1, 2, 3, 4, 5])
        expect(h.invoke.mock.calls.map(([command]) => command)).toEqual([
            'pds_commit_shared_open',
            'pds_commit_shared_chunk',
            'pds_commit_shared_chunk',
            'pds_commit_shared_finish',
            'pds_commit_shared_cancel',
        ])
        expect(h.webview.releaseBuffer).toHaveBeenCalledOnce()
        expect(h.webview.removeEventListener).toHaveBeenCalledOnce()
    })
    it('falls back to Worker raw only when shared support is unavailable before commit', async () => {
        const h = harness({ unsupported: true })
        await h.transport.commit(fixture())
        expect(h.invoke.mock.calls.map(([command]) => command)).toEqual([
            'pds_commit_shared_open',
            'pds_commit_raw',
        ])
    })
    it.each(['pds_commit_shared_chunk', 'pds_commit_shared_finish'])(
        'never replays a failed %s and permits the next request',
        async (fail) => {
            const h = harness({ fail })
            await expect(h.transport.commit(fixture())).rejects.toThrow('native failed')
            expect(h.invoke.mock.calls.map(([command]) => command)).not.toContain('pds_commit_raw')
            expect(h.webview.releaseBuffer).toHaveBeenCalledOnce()
            expect(h.invoke).toHaveBeenCalledWith('pds_commit_shared_cancel', {
                id: 'session',
            })
            await h.transport.commit(fixture(false))
            expect(h.invoke).toHaveBeenLastCalledWith('pds_commit', fixture(false))
        },
    )
    it('rejects an incorrect acknowledgement without submitting the commit', async () => {
        const h = harness({ ack: 100 })
        await expect(h.transport.commit(fixture())).rejects.toThrow('acknowledgement')
        expect(h.invoke.mock.calls.map(([command]) => command)).not.toContain(
            'pds_commit_shared_finish',
        )
    })
    it('serializes overlapping requests even while Worker encoding is pending', async () => {
        const h = harness()
        let release!: (bytes: Uint8Array<ArrayBuffer>) => void
        h.encode.mockImplementationOnce(
            () =>
                new Promise((resolve) => {
                    release = resolve
                }),
        )
        const first = h.transport.commit(fixture())
        const second = h.transport.commit(fixture(false))
        await Promise.resolve()
        expect(h.invoke).not.toHaveBeenCalled()
        release(new Uint8Array([1]))
        await Promise.all([first, second])
        expect(h.invoke.mock.calls.at(-1)?.[0]).toBe('pds_commit')
    })
})
