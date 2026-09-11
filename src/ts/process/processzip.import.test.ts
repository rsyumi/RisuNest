// @vitest-environment happy-dom
import { describe, expect, it, vi } from 'vitest'
import { strToU8, zipSync } from 'fflate'
import { CharXImporter } from './processzip'

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
