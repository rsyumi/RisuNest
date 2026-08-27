import { describe, expect, it, vi } from 'vitest'

vi.mock('../alert', () => ({ alertConfirm: vi.fn() }))
vi.mock('../../lang', () => ({ language: { lowLevelAccessConfirm: 'confirm' } }))
vi.mock('./persistentDataRuntime.svelte', () => ({ appendPersistentRootModule: vi.fn() }))

import type {
    PreparedNativeContent,
    PreparedNativeContentActivationLifecycle,
} from './nativeFileJobs'
import {
    activatePreparedNativeModuleContent,
    type PreparedRootModuleAppend,
} from './nativeModuleContentActivation'
import { runNativePreparedContentRoute } from './nativePreparedContentRoute'
import { PersistentRootModuleAppendRejectedError } from './saveCoordinator'

const hash = (byte: string) => byte.repeat(64)

function risumContent(assets: unknown = [
    ['same', '', 'PNG'],
    ['same', '', 'bin'],
]): PreparedNativeContent {
    return {
        casSessionId: 'content-job',
        format: 'risu-module',
        metadata: {
            id: 'source-id',
            name: 'Imported module',
            lowLevelAccess: true,
            unknownFutureField: { retained: true },
            ...(assets === undefined ? {} : { assets }),
        },
        assets: assets === undefined ? [] : [
            {
                position: 0,
                logicalId: `assets/${hash('a')}.PNG`,
                objectHash: hash('a'),
                byteSize: 4,
                mime: '',
                name: '',
                ext: 'PNG',
            },
            {
                position: 1,
                logicalId: `assets/${hash('b')}.bin`,
                objectHash: hash('b'),
                byteSize: 0,
                mime: '',
                name: '',
                ext: 'bin',
            },
        ],
        ownerHead: assets === undefined
            ? { present: false, manifestHash: null, entryCount: 0 }
            : { present: true, manifestHash: hash('c'), entryCount: 2 },
    }
}

describe('native RISUM module activation', () => {
    it('confirms low-level access, seals native roots, then atomically appends full metadata', async () => {
        const events: string[] = []
        const append = vi.fn(async () => { events.push('append') })
        const lifecycle: PreparedNativeContentActivationLifecycle = {
            prepareOwnerManifestAndSeal: vi.fn(),
            sealPreparedContent: vi.fn(async () => { events.push('seal') }),
        }

        const result = await activatePreparedNativeModuleContent(
            risumContent(),
            lifecycle,
            {
                confirmLowLevelAccess: vi.fn(async () => {
                    events.push('confirm')
                    return true
                }),
                createId: () => 'new-module-id',
                append,
            },
        )

        expect(events).toEqual(['confirm', 'seal', 'append'])
        expect(append).toHaveBeenCalledWith({
            module: {
                id: 'new-module-id',
                name: 'Imported module',
                lowLevelAccess: true,
                unknownFutureField: { retained: true },
                assets: [
                    ['same', `assets/${hash('a')}.PNG`, 'PNG'],
                    ['same', `assets/${hash('b')}.bin`, 'bin'],
                ],
            },
            assetAliases: [
                {
                    kind: 'asset',
                    key: `assets/${hash('a')}.PNG`,
                    objectHash: hash('a'),
                    size: 4,
                    mime: '',
                    name: '',
                    ext: 'PNG',
                },
                {
                    kind: 'asset',
                    key: `assets/${hash('b')}.bin`,
                    objectHash: hash('b'),
                    size: 0,
                    mime: '',
                    name: '',
                    ext: 'bin',
                },
            ],
            ownerHead: {
                present: true,
                manifestHash: hash('c'),
                entryCount: 2,
            },
        }, undefined)
        expect(result).toEqual({ moduleId: 'new-module-id' })
    })

    it('preserves tuple trailing fields and deduplicates identical logical aliases', async () => {
        const content = risumContent([
            ['same', '', '.unsafe/path', { future: true }],
            ['same', '', '.unsafe/path', 'tail'],
        ])
        content.assets[0] = {
            ...content.assets[0],
            logicalId: `assets/${hash('a')}.bin`,
            ext: '.unsafe/path',
        }
        content.assets[1] = {
            ...content.assets[0],
            position: 1,
        }
        const append = vi.fn(async (_input: PreparedRootModuleAppend) => undefined)

        await activatePreparedNativeModuleContent(content, {
            prepareOwnerManifestAndSeal: vi.fn(),
            sealPreparedContent: vi.fn(async () => undefined),
        }, {
            confirmLowLevelAccess: vi.fn(async () => true),
            createId: () => 'new-id',
            append,
        })

        const input = append.mock.calls[0][0]
        expect(input.module.assets).toEqual([
            ['same', `assets/${hash('a')}.bin`, '.unsafe/path', { future: true }],
            ['same', `assets/${hash('a')}.bin`, '.unsafe/path', 'tail'],
        ])
        expect(input.assetAliases).toEqual([{
            kind: 'asset',
            key: `assets/${hash('a')}.bin`,
            objectHash: hash('a'),
            size: 4,
            mime: '',
            name: '',
            ext: '.unsafe/path',
        }])
    })

    it('preserves an absent assets property and does not seal after rejected confirmation', async () => {
        const lifecycle: PreparedNativeContentActivationLifecycle = {
            prepareOwnerManifestAndSeal: vi.fn(),
            sealPreparedContent: vi.fn(),
        }
        const append = vi.fn()

        await expect(activatePreparedNativeModuleContent(
            risumContent(undefined),
            lifecycle,
            {
                confirmLowLevelAccess: vi.fn(async () => false),
                createId: () => 'unused',
                append,
            },
        )).resolves.toBeNull()

        expect(lifecycle.sealPreparedContent).not.toHaveBeenCalled()
        expect(append).not.toHaveBeenCalled()
    })

    it('releases sealed roots and does not append when cancellation arrives during sealing', async () => {
        const controller = new AbortController()
        const reason = new DOMException('cancelled during sealing', 'AbortError')
        const cleanupError = new Error('native cleanup unavailable')
        const append = vi.fn()
        const lifecycle: PreparedNativeContentActivationLifecycle = {
            prepareOwnerManifestAndSeal: vi.fn(),
            sealPreparedContent: vi.fn(async () => controller.abort(reason)),
            abortPreparedContent: vi.fn(async () => { throw cleanupError }),
        }

        await expect(activatePreparedNativeModuleContent(
            risumContent(undefined),
            lifecycle,
            {
                confirmLowLevelAccess: vi.fn(async () => true),
                createId: () => 'unused',
                append,
            },
            controller.signal,
        )).rejects.toBe(reason)

        expect(lifecycle.abortPreparedContent).toHaveBeenCalledOnce()
        expect(append).not.toHaveBeenCalled()
    })

    it('threads cancellation into the persistent append and releases sealed roots', async () => {
        const controller = new AbortController()
        const reason = new DOMException('cancelled during alias read', 'AbortError')
        const lifecycle: PreparedNativeContentActivationLifecycle = {
            prepareOwnerManifestAndSeal: vi.fn(),
            sealPreparedContent: vi.fn(async () => undefined),
            abortPreparedContent: vi.fn(async () => undefined),
        }
        const append = vi.fn(async (_input: PreparedRootModuleAppend, signal?: AbortSignal) => {
            expect(signal).toBe(controller.signal)
            controller.abort(reason)
            signal?.throwIfAborted()
        })

        await expect(activatePreparedNativeModuleContent(
            risumContent(undefined),
            lifecycle,
            {
                confirmLowLevelAccess: vi.fn(async () => true),
                createId: () => 'new-id',
                append,
            },
            controller.signal,
        )).rejects.toBe(reason)

        expect(lifecycle.sealPreparedContent).toHaveBeenCalledOnce()
        expect(lifecycle.abortPreparedContent).toHaveBeenCalledOnce()
    })

    it('aborts the sealed native session when the atomic revision commit loses its CAS', async () => {
        const rejection = new PersistentRootModuleAppendRejectedError('revision conflict')
        const receipt = {
            jobId: 'content-job',
            content: risumContent(undefined),
            warningCodes: [],
            prepareOwnerManifestAndSeal: vi.fn(),
            sealPreparedContent: vi.fn(async () => undefined),
            abortPreparedContent: vi.fn(async () => { throw new Error('native cleanup unavailable') }),
            confirmActivated: vi.fn(async () => undefined),
            cancel: vi.fn(async () => undefined),
        }

        await expect(runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\module.risum' },
            'module.risum',
            {
                prepare: vi.fn(async () => receipt),
                map: async (content) => content,
                activate: (content, lifecycle) => activatePreparedNativeModuleContent(
                    content,
                    lifecycle,
                    {
                        confirmLowLevelAccess: vi.fn(async () => true),
                        createId: () => 'new-id',
                        append: vi.fn(async () => {
                            throw rejection
                        }),
                    },
                ),
            },
        )).rejects.toBe(rejection)

        expect(receipt.sealPreparedContent).toHaveBeenCalledOnce()
        expect(receipt.abortPreparedContent).toHaveBeenCalledOnce()
        expect(receipt.cancel).toHaveBeenCalledOnce()
        expect(receipt.confirmActivated).not.toHaveBeenCalled()
    })

    it('retains sealed roots when the append response is ambiguous', async () => {
        const lifecycle: PreparedNativeContentActivationLifecycle = {
            prepareOwnerManifestAndSeal: vi.fn(),
            sealPreparedContent: vi.fn(async () => undefined),
            abortPreparedContent: vi.fn(async () => undefined),
        }

        await expect(activatePreparedNativeModuleContent(
            risumContent(undefined),
            lifecycle,
            {
                confirmLowLevelAccess: vi.fn(async () => true),
                createId: () => 'new-id',
                append: vi.fn(async () => { throw new Error('commit response lost') }),
            },
        )).rejects.toThrow('commit response lost')

        expect(lifecycle.abortPreparedContent).not.toHaveBeenCalled()
    })
})
