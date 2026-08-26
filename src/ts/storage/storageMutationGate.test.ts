import { describe, expect, test } from 'vitest'
import { createInRealmStorageLockManager, createStorageMutationGate } from './storageMutationGate'

const deferred = () => {
    let resolve!: () => void
    const promise = new Promise<void>((done) => { resolve = done })
    return { promise, resolve }
}

describe('storage mutation gate', () => {
    test('lets concurrent writes from separate clients overlap', async () => {
        const locks = createInRealmStorageLockManager()
        const first = createStorageMutationGate({ locks })
        const second = createStorageMutationGate({ locks })
        const firstRelease = deferred()
        const events: string[] = []

        const firstWrite = first.runWrite(async () => {
            events.push('write-1-start')
            await firstRelease.promise
            events.push('write-1-end')
        })
        const secondWrite = second.runWrite(async () => {
            events.push('write-2')
        })

        await Promise.resolve()
        expect(events).toEqual(['write-1-start', 'write-2'])
        firstRelease.resolve()
        await Promise.all([firstWrite, secondWrite])
        expect(events).toEqual(['write-1-start', 'write-2', 'write-1-end'])
    })

    test('serializes keyed writes for the same Inlay across clients', async () => {
        const locks = createInRealmStorageLockManager()
        const first = createStorageMutationGate({ locks })
        const second = createStorageMutationGate({ locks })
        const firstRelease = deferred()
        const events: string[] = []

        const firstWrite = first.runKeyedWrite('inlay-a', async () => {
            events.push('first-start')
            await firstRelease.promise
            events.push('first-end')
        })
        const secondWrite = second.runKeyedWrite('inlay-a', async () => {
            events.push('second')
        })

        await Promise.resolve()
        await Promise.resolve()
        expect(events).toEqual(['first-start'])
        firstRelease.resolve()
        await Promise.all([firstWrite, secondWrite])
        expect(events).toEqual(['first-start', 'first-end', 'second'])
    })

    test('lets keyed writes for different Inlays overlap', async () => {
        const locks = createInRealmStorageLockManager()
        const gate = createStorageMutationGate({ locks })
        const firstRelease = deferred()
        const events: string[] = []

        const firstWrite = gate.runKeyedWrite('inlay-a', async () => {
            events.push('first-start')
            await firstRelease.promise
        })
        const secondWrite = gate.runKeyedWrite('inlay-b', async () => {
            events.push('second')
        })

        await Promise.resolve()
        await Promise.resolve()
        expect(events).toEqual(['first-start', 'second'])
        firstRelease.resolve()
        await Promise.all([firstWrite, secondWrite])
    })

    test('releases the lock when a write throws synchronously', async () => {
        const gate = createStorageMutationGate({ locks: createInRealmStorageLockManager() })
        const error = new Error('synchronous failure')

        await expect(gate.runWrite(() => { throw error })).rejects.toBe(error)
        await expect(Promise.race([
            gate.runWrite(async () => 'continued'),
            new Promise<string>((resolve) => setTimeout(() => resolve('stranded'), 50)),
        ])).resolves.toBe('continued')
    })

    test('still gates writes where Web Locks are unavailable', async () => {
        const gate = createStorageMutationGate()
        await expect(gate.runWrite(async () => 'written')).resolves.toBe('written')
    })
})
