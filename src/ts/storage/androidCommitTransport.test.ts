import { describe, expect, it, vi } from 'vitest'
import { sendAndroidCommit, ANDROID_COMMIT_CHUNK_BYTES } from './androidCommitTransport'
import {
    ANDROID_BINARY_CHUNK_BYTES,
    type AndroidBinaryCommitBridge,
} from './androidBinaryCommitBridge'

function harness(options: { fail?: string; badAck?: boolean } = {}) {
    const received: string[] = []
    const invoke = vi.fn(async (command: string, args: any) => {
        if (command === options.fail) throw new Error('synthetic failure')
        if (command === 'pds_commit_android_open') return { capacity: ANDROID_COMMIT_CHUNK_BYTES }
        if (command === 'pds_commit_android_chunk') {
            received.push(args.chunk)
            return (
                args.offset + new TextEncoder().encode(args.chunk).length + (options.badAck ? 1 : 0)
            )
        }
        return { revision: 3 }
    })
    return { invoke, received }
}

describe('Android bounded commit transport', () => {
    function binaryHarness(
        options: {
            badAck?: boolean
            drop?: boolean
            reject?: boolean
            finishFails?: boolean
        } = {},
    ) {
        const packets: Uint8Array[] = []
        const bridge: AndroidBinaryCommitBridge = {
            onmessage: null,
            postMessage(packet) {
                const bytes = new Uint8Array(packet)
                packets.push(bytes.slice(40))
                const id = new TextDecoder().decode(bytes.subarray(0, 36))
                const offset = new DataView(packet).getUint32(36, true) + bytes.length - 40
                if (!options.drop)
                    queueMicrotask(() =>
                        bridge.onmessage?.({
                            data: JSON.stringify({
                                id,
                                offset: offset + (options.badAck ? 1 : 0),
                                error: options.reject ? 'invalid' : undefined,
                            }),
                        }),
                    )
            },
        }
        const invoke = vi.fn(async (command: string) => {
            if (command === 'pds_commit_android_open')
                return { capacity: ANDROID_BINARY_CHUNK_BYTES }
            if (command === 'pds_commit_android_finish' && options.finishFails)
                throw new Error('uncertain finish')
            return { revision: 4 }
        })
        return { bridge, invoke, packets }
    }
    it('uses exact bounded binary chunks, including split UTF-8, and releases the listener', async () => {
        const h = binaryHarness()
        const bytes = new TextEncoder().encode(
            'a'.repeat(ANDROID_BINARY_CHUNK_BYTES - 1) + '🐿️합성'.repeat(50000),
        )
        expect(await sendAndroidCommit(bytes, h.invoke as any, h.bridge)).toEqual({
            revision: 4,
        })
        expect(new Uint8Array(h.packets.flatMap((p) => Array.from(p)))).toEqual(bytes)
        expect(h.packets.every((p) => p.length <= ANDROID_BINARY_CHUNK_BYTES)).toBe(true)
        expect(h.invoke).toHaveBeenCalledWith(
            'pds_commit_android_open',
            expect.objectContaining({ binary: true }),
        )
        expect(h.invoke.mock.calls.map(([name]) => name)).toEqual([
            'pds_commit_android_open',
            'pds_commit_android_finish',
            'pds_commit_android_cancel',
        ])
        expect(h.bridge.onmessage).toBeNull()
    })
    it.each([{ badAck: true }, { reject: true }, { finishFails: true }])(
        'does not replay failed binary transfer %j',
        async (options) => {
            const h = binaryHarness(options)
            await expect(
                sendAndroidCommit(new TextEncoder().encode('{}'), h.invoke as any, h.bridge),
            ).rejects.toThrow()
            expect(h.bridge.onmessage).toBeNull()
            expect(h.invoke.mock.calls.at(-1)?.[0]).toBe('pds_commit_android_cancel')
            expect(
                h.invoke.mock.calls.some(([name]) =>
                    ['pds_commit', 'pds_commit_android_chunk'].includes(name),
                ),
            ).toBe(false)
            expect(
                h.invoke.mock.calls.filter(([name]) => name === 'pds_commit_android_finish'),
            ).toHaveLength(options.finishFails ? 1 : 0)
        },
    )
    it('times out a missing ACK, cancels, and does not retry through strings', async () => {
        vi.useFakeTimers()
        try {
            const h = binaryHarness({ drop: true })
            const result = sendAndroidCommit(
                new TextEncoder().encode('{}'),
                h.invoke as any,
                h.bridge,
            )
            const rejected = expect(result).rejects.toThrow('timed out')
            await vi.advanceTimersByTimeAsync(10001)
            await rejected
            expect(h.bridge.onmessage).toBeNull()
            expect(h.invoke.mock.calls.map(([name]) => name)).toEqual([
                'pds_commit_android_open',
                'pds_commit_android_cancel',
            ])
        } finally {
            vi.useRealTimers()
        }
    })
    it('keeps the string route when no native binary listener is available', async () => {
        const h = harness()
        await sendAndroidCommit(new TextEncoder().encode('{}'), h.invoke, null)
        expect(h.invoke).toHaveBeenCalledWith(
            'pds_commit_android_open',
            expect.objectContaining({ binary: false }),
        )
        expect(h.received).toEqual(['{}'])
    })
    it('preserves exact UTF-8 across every possible multibyte boundary', async () => {
        for (const char of ['é', '합', '🐿', '\\"\n', '\ufeff']) {
            for (let shift = 0; shift <= 4; shift++) {
                const text = 'a'.repeat(ANDROID_COMMIT_CHUNK_BYTES - shift) + char.repeat(5)
                const h = harness()
                expect(await sendAndroidCommit(new TextEncoder().encode(text), h.invoke)).toEqual({
                    revision: 3,
                })
                expect(h.received.join('')).toBe(text)
                expect(
                    h.received.every(
                        (chunk) =>
                            new TextEncoder().encode(chunk).length <= ANDROID_COMMIT_CHUNK_BYTES,
                    ),
                ).toBe(true)
                expect(h.invoke.mock.calls.map(([command]) => command).slice(-2)).toEqual([
                    'pds_commit_android_finish',
                    'pds_commit_android_cancel',
                ])
            }
        }
    })
    it.each(['pds_commit_android_open', 'pds_commit_android_chunk', 'pds_commit_android_finish'])(
        'cancels %s failure without replaying an uncertain save',
        async (fail) => {
            const h = harness({ fail })
            await expect(
                sendAndroidCommit(new TextEncoder().encode('{}'), h.invoke),
            ).rejects.toThrow('synthetic failure')
            expect(h.invoke).toHaveBeenLastCalledWith('pds_commit_android_cancel', {
                id: h.invoke.mock.calls[0][1].id,
            })
            expect(
                h.invoke.mock.calls.some(
                    ([cmd]) => cmd === 'pds_commit' || cmd === 'pds_commit_raw',
                ),
            ).toBe(false)
        },
    )
    it('rejects a wrong acknowledgement before commit', async () => {
        const h = harness({ badAck: true })
        await expect(sendAndroidCommit(new TextEncoder().encode('{}'), h.invoke)).rejects.toThrow(
            'acknowledgement',
        )
        expect(h.invoke.mock.calls.some(([cmd]) => cmd === 'pds_commit_android_finish')).toBe(false)
    })
    it('rejects an invalid capacity and malformed UTF-8 without a save', async () => {
        const h = harness()
        h.invoke.mockResolvedValueOnce({ capacity: 0 } as any)
        await expect(sendAndroidCommit(new Uint8Array([123, 125]), h.invoke)).rejects.toThrow(
            'capacity',
        )
        await expect(sendAndroidCommit(new Uint8Array([0xff]), h.invoke)).rejects.toThrow()
        expect(h.invoke.mock.calls.some(([cmd]) => cmd === 'pds_commit_android_finish')).toBe(false)
    })
})
