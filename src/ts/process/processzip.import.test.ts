// @vitest-environment happy-dom
import { Buffer } from 'buffer'
import { describe, expect, it, vi } from 'vitest'
import { strToU8, zipSync } from 'fflate'
import { CharXImporter, CharXWriter } from './processzip'

vi.mock('../globalApi.svelte', async () => ({
    ...(await import('../appendableBuffer')),
    saveAsset: vi.fn(),
}))
vi.mock('../alert', () => ({ alertStore: { set: vi.fn() } }))
vi.mock('../util', () => ({
    Semaphore: class {
        constructor(private available: number) {}
        private waiters: (() => void)[] = []
        async acquire() {
            if (this.available > 0) {
                this.available--
                return
            }
            await new Promise<void>((resolve) => this.waiters.push(resolve))
        }
        release() {
            const next = this.waiters.shift()
            if (next) next()
            else this.available++
        }
    },
}))

describe('large CharX fallback', () => {
    it('round-trips card metadata and asset bytes through the JavaScript writer and reader', async () => {
        const chunks: Uint8Array[] = []
        const writer = new CharXWriter({
            write: async (data: Uint8Array) => { chunks.push(data.slice()) },
            close: async () => undefined,
        } as any)
        const card = '{"name":"Synthetic card"}'
        const asset = Uint8Array.of(0, 255, 7, 128)

        await writer.init()
        await writer.write('card.json', card)
        await writer.write('assets/avatar.png', asset)
        await writer.end()

        const stored: Uint8Array[] = []
        const importer = new CharXImporter(async (data) => {
            stored.push(data.slice())
            return 'assets/synthetic'
        })
        await importer.parse(Uint8Array.from(Buffer.concat(chunks)))
        await importer.done()

        expect(importer.cardData).toBe(card)
        expect(stored).toEqual([asset])
        expect(importer.assets).toEqual({ 'assets/avatar.png': 'assets/synthetic' })
    })

    it('pauses decoding behind storage and imports 10,000 highly compressed entries without recursive overflow', async () => {
        const entries: Record<string, Uint8Array> = {}
        for (let i = 0; i < 10_000; i++)
            entries[`assets/${i}.bin`] = new Uint8Array(4096).fill(i % 256)
        entries['card.json'] = strToU8('{"synthetic":true}')
        let release!: () => void
        const gate = new Promise<void>((resolve) => {
            release = resolve
        })
        let saved = 0
        const importer = new CharXImporter(async () => {
            await gate
            return `assets/${saved++}`
        })
        const parsing = importer.parse(zipSync(entries))
        await new Promise((resolve) => setTimeout(resolve, 10))
        expect(importer.cardData).toBeUndefined()
        expect(saved).toBe(0)
        release()
        await parsing
        await importer.done()
        expect(saved).toBe(10_000)
        expect(Object.keys(importer.assets)).toHaveLength(10_000)
        expect(importer.cardData).toBe('{"synthetic":true}')
    }, 20_000)

    it('propagates a storage failure without waiting forever or leaving a rejected completion promise', async () => {
        const importer = new CharXImporter(async () => {
            throw new Error('disk full')
        })
        const input = zipSync({
            'assets/one.bin': new Uint8Array(32),
            'card.json': strToU8('{}'),
        })
        // A last-chunk failure is reported by done; an earlier one may stop parse.
        await expect(
            (async () => {
                await importer.parse(input)
                await importer.done()
            })(),
        ).rejects.toThrow('disk full')
    })
})
