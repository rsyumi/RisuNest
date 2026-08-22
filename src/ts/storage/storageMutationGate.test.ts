import { describe, expect, test } from 'vitest'
import { createInRealmStorageLockManager, createStorageMutationGate } from './storageMutationGate'

const deferred = () => {
    let resolve!: () => void
    const promise = new Promise<void>((done) => { resolve = done })
    return { promise, resolve }
}

describe('storage mutation gate', () => {
    test('allows shared writes together and orders an exclusive migration before later writes', async () => {
        const locks = createInRealmStorageLockManager()
        const first = createStorageMutationGate({ locks })
        const second = createStorageMutationGate({ locks })
        const firstRelease = deferred()
        const secondRelease = deferred()
        const events: string[] = []

        const firstWrite = first.runWrite(async () => {
            events.push('write-1-start')
            await firstRelease.promise
            events.push('write-1-end')
        })
        const secondWrite = second.runWrite(async () => {
            events.push('write-2-start')
            await secondRelease.promise
            events.push('write-2-end')
        })
        await Promise.resolve()
        const migration = first.runMigration(async () => {
            events.push('migration')
        })
        const laterWrite = second.runWrite(async () => {
            events.push('write-3')
        })

        await Promise.resolve()
        expect(events).toEqual(['write-1-start', 'write-2-start'])
        firstRelease.resolve()
        secondRelease.resolve()
        await Promise.all([firstWrite, secondWrite, migration, laterWrite])
        expect(events).toEqual([
            'write-1-start', 'write-2-start', 'write-1-end', 'write-2-end', 'migration', 'write-3',
        ])
    })

    test('serializes migrations across clients and releases after failure', async () => {
        const locks = createInRealmStorageLockManager()
        const first = createStorageMutationGate({ locks })
        const second = createStorageMutationGate({ locks })
        const release = deferred()
        let overlap = 0
        let maximum = 0
        const failed = first.runMigration(async () => {
            overlap++
            maximum = Math.max(maximum, overlap)
            await release.promise
            overlap--
            throw new Error('stage failed')
        })
        const next = second.runMigration(async () => {
            overlap++
            maximum = Math.max(maximum, overlap)
            overlap--
            return 'continued'
        })

        release.resolve()
        await expect(failed).rejects.toThrow('stage failed')
        await expect(next).resolves.toBe('continued')
        expect(maximum).toBe(1)
    })

    test.each([
        ['shared write', 'runWrite', 'runMigration'],
        ['exclusive migration', 'runMigration', 'runWrite'],
    ] as const)('releases a %s lock when its callback throws synchronously', async (_name, failing, following) => {
        const gate = createStorageMutationGate({ locks: createInRealmStorageLockManager() })
        const error = new Error('synchronous failure')

        await expect(gate[failing](() => { throw error })).rejects.toBe(error)
        await expect(Promise.race([
            gate[following](async () => 'continued'),
            new Promise<string>((resolve) => setTimeout(() => resolve('stranded'), 50)),
        ])).resolves.toBe('continued')
    })
})
