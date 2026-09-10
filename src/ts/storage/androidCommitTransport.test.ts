import { describe, expect, it, vi } from 'vitest'
import { sendAndroidCommit, ANDROID_COMMIT_CHUNK_BYTES } from './androidCommitTransport'

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
