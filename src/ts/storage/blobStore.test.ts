import { describe, expect, test } from 'vitest'
import { createKeyValueBlobStore, type BlobKeyValueBackend } from './blobStore'

function harness() {
    const values = new Map<string, Uint8Array>()
    let payloadReads = 0
    const backend: BlobKeyValueBackend = {
        async write(key, value) {
            values.set(key, value.slice())
        },
        async read(key) {
            if (!key.includes('/metadata/')) payloadReads += 1
            return values.get(key)?.slice() ?? null
        },
        async keys() {
            return [...values.keys()]
        },
        async remove(key) {
            values.delete(key)
        },
    }
    return {
        store: createKeyValueBlobStore(backend, { kind: 'legacy' }),
        values,
        payloadReadCount: () => payloadReads,
    }
}

describe('BlobStore contract', () => {
    test('distinguishes missing and empty payloads', async () => {
        const { store } = harness()
        expect(await store.read('assets/missing')).toBeNull()
        expect(await store.stat('assets/missing')).toBeNull()

        await store.put('assets/empty', new Uint8Array(), {
            kind: 'asset', mime: 'application/octet-stream', name: 'empty', ext: '',
        })
        expect(await store.read('assets/empty')).toEqual(new Uint8Array())
        expect(await store.read('assets/empty', { start: 0, endExclusive: 0 })).toEqual(new Uint8Array())
        expect((await store.stat('assets/empty'))?.size).toBe(0)
    })

    test('uses validated end-exclusive ranges', async () => {
        const { store } = harness()
        await store.put('assets/range.bin', new Uint8Array([0, 1, 2, 3]), {
            kind: 'asset', mime: 'application/octet-stream', name: 'range.bin', ext: 'BIN',
        })
        expect(await store.read('assets/range.bin')).toEqual(new Uint8Array([0, 1, 2, 3]))
        expect(await store.read('assets/range.bin', { start: 0, endExclusive: 2 })).toEqual(new Uint8Array([0, 1]))
        expect(await store.read('assets/range.bin', { start: 2, endExclusive: 9 })).toEqual(new Uint8Array([2, 3]))
        expect(await store.read('assets/range.bin', { start: 4, endExclusive: 8 })).toEqual(new Uint8Array())
        for (const range of [
            { start: -1, endExclusive: 1 },
            { start: 0.5, endExclusive: 1 },
            { start: 0, endExclusive: Number.POSITIVE_INFINITY },
            { start: 2, endExclusive: 1 },
        ]) {
            await expect(store.read('assets/range.bin', range)).rejects.toBeInstanceOf(RangeError)
        }
    })

    test('records normalized metadata and lists without payload reads', async () => {
        const { store, payloadReadCount } = harness()
        await store.put('raw/id', new Uint8Array([7, 8]), {
            kind: 'inlay', inlayType: 'image', mime: 'image/png', name: 'Photo.PNG', ext: '.PNG', width: 4, height: 5,
        })
        await store.put('assets/photo.jpg', new Uint8Array([1]), {
            kind: 'asset', mime: 'image/jpeg', name: 'photo.jpg', ext: '.JPG',
        })
        const readsBeforeList = payloadReadCount()
        expect(await store.list()).toMatchObject([
            { key: 'assets/photo.jpg', kind: 'asset', ext: 'jpg', size: 1 },
            { key: 'raw/id', kind: 'inlay', ext: 'png', size: 2, width: 4, height: 5 },
        ])
        expect(await store.list({ kind: 'inlay' })).toHaveLength(1)
        expect(payloadReadCount()).toBe(readsBeforeList)
    })

    test('remove is idempotent and removes payload and metadata', async () => {
        const { store } = harness()
        await store.put('raw/id', new Uint8Array([1]), {
            kind: 'inlay', inlayType: 'signature', mime: 'application/json', name: 'id', ext: 'json',
        })
        await store.remove('raw/id')
        await store.remove('raw/id')
        expect(await store.read('raw/id')).toBeNull()
        expect(await store.stat('raw/id')).toBeNull()
    })

    test('backfills legacy asset metadata without rewriting payloads', async () => {
        const { store, values } = harness()
        const payload = new Uint8Array([4, 5, 6])
        values.set('assets/legacy.MP3', payload.slice())
        expect(await store.list({ kind: 'asset' })).toMatchObject([
            { key: 'assets/legacy.MP3', ext: 'mp3', mime: 'audio/mpeg', size: 3 },
        ])
        expect(values.get('assets/legacy.MP3')).toEqual(payload)
    })

    test('ignores dangling metadata without reading payload bytes', async () => {
        const { store, values, payloadReadCount } = harness()
        values.set('blobstore/metadata/6173736574732f67686f7374.json', new TextEncoder().encode(JSON.stringify({
            key: 'assets/ghost', kind: 'asset', size: 4, mime: 'image/png', name: 'ghost', ext: 'png',
        })))
        expect(await store.stat('assets/ghost')).toBeNull()
        const readsBeforeList = payloadReadCount()
        expect(await store.list()).toEqual([])
        expect(payloadReadCount()).toBe(readsBeforeList)
    })
})
