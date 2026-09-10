import { describe, expect, it, vi } from 'vitest'
import { MAX_CONTENT_METADATA_BYTES } from './contentImportLimits'

import {
    prepareNativeContentImport,
    type NativeFileJobDependencies,
    type NativeFileJobStatus,
    type PreparedNativeContent,
} from './nativeFileJobs'
import { runNativePreparedContentRoute } from './nativePreparedContentRoute'

const preparedContent: PreparedNativeContent = {
    casSessionId: 'content-1',
    format: 'json-card',
    metadata: {
        spec: 'chara_card_v3',
        spec_version: '3.0',
        data: { name: 'Native Card' },
    },
    assets: [{
        referenceKey: 'data.assets.0.uri',
        token: 'staged-token-1',
        logicalId: `assets/${'ab'.repeat(32)}.png`,
        objectHash: 'ab'.repeat(32),
        byteSize: 12,
        mime: 'image/png',
        name: 'portrait.png',
        ext: 'png',
    }],
}

const preparedRisumContent = {
    casSessionId: 'content-1',
    format: 'risu-module',
    metadata: {
        id: 'source-id',
        name: 'Native module',
        unknownFutureField: { retained: true },
        assets: [
            ['duplicate', '', '.unsafe/path', { future: true }],
            ['duplicate', '', 'bin'],
        ],
    },
    assets: [
        {
            position: 0,
            logicalId: `assets/${'ab'.repeat(32)}.PNG`,
            objectHash: 'ab'.repeat(32),
            byteSize: 4,
            mime: '',
            name: '',
            ext: '.unsafe/path',
        },
        {
            position: 1,
            logicalId: `assets/${'cd'.repeat(32)}.bin`,
            objectHash: 'cd'.repeat(32),
            byteSize: 0,
            mime: '',
            name: '',
            ext: 'bin',
        },
    ],
    ownerHead: {
        present: true,
        manifestHash: 'ef'.repeat(32),
        entryCount: 2,
    },
} as const

function contentStatus(
    state: NativeFileJobStatus['state'],
    phase: NativeFileJobStatus['phase'],
    content?: PreparedNativeContent,
): NativeFileJobStatus {
    return {
        jobId: 'content-1',
        kind: 'prepare-content-import',
        state,
        phase,
        progress: { completedBytes: 12, totalBytes: 12, completedItems: 1, totalItems: 1 },
        preparedContent: content,
    }
}

function nativeDependencies(
    invoke: NativeFileJobDependencies['invoke'],
    wait: NativeFileJobDependencies['wait'] = async () => undefined,
): NativeFileJobDependencies {
    return { isTauri: () => true, invoke, wait }
}

describe('native prepared content import', () => {
    it('accepts an occurrence-based RISUM receipt and seals it using only the job id', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\module.risum' },
            'module.risum',
            {},
            nativeDependencies(async (command, args) => {
                calls.push([command, args])
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus(
                        'succeeded',
                        'complete',
                        preparedRisumContent as unknown as PreparedNativeContent,
                    )
                }
                if (
                    command === 'asset_cas_job_seal_prepared_content'
                    || command === 'asset_cas_job_release'
                    || command === 'native_file_job_forget'
                ) return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        expect(receipt.content).toEqual(preparedRisumContent)
        await receipt.sealPreparedContent()
        await receipt.confirmActivated()

        expect(calls.slice(2)).toEqual([
            ['asset_cas_job_seal_prepared_content', { sessionId: 'content-1' }],
            ['asset_cas_job_release', { sessionId: 'content-1', outcome: 'committed' }],
            ['native_file_job_forget', { jobId: 'content-1' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('ownerManifest')
        expect(JSON.stringify(calls)).not.toContain('contentAssets')
        expect(JSON.stringify(calls.slice(2))).not.toContain('path')
    })

    it('releases a sealed prepared session when activation proves it was not committed', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\module.risum' },
            'module.risum',
            {},
            nativeDependencies(async (command, args) => {
                calls.push([command, args])
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedRisumContent as unknown as PreparedNativeContent)
                }
                if (
                    command === 'asset_cas_job_seal_prepared_content'
                    || command === 'asset_cas_job_release'
                    || command === 'native_file_job_forget'
                ) return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        await receipt.sealPreparedContent()
        await receipt.abortPreparedContent!()

        expect(calls.slice(2)).toEqual([
            ['asset_cas_job_seal_prepared_content', { sessionId: 'content-1' }],
            ['asset_cas_job_release', { sessionId: 'content-1', outcome: 'aborted' }],
            ['native_file_job_forget', { jobId: 'content-1' }],
        ])
    })

    it('returns a validated metadata-only receipt and retains the native job', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command, args) => {
                calls.push([command, args])
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        expect(receipt).toMatchObject({
            jobId: 'content-1',
            content: preparedContent,
        })
        expect(calls).toEqual([
            ['native_file_job_start', {
                request: {
                    kind: 'prepare-content-import',
                    source: { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
                    displayName: 'card.json',
                },
            }],
            ['native_file_job_status', { jobId: 'content-1' }],
        ])
        expect(calls.map(([command]) => command)).not.toContain('native_file_job_forget')
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('finalizes one owner manifest without descriptor echo, commits, then forgets', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const manifestHash = 'cd'.repeat(32)
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command, args) => {
                calls.push([command, args])
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_finalize_content') return {
                    contentHash: manifestHash,
                    byteSize: 3,
                    physicalKey: `assets-v2/objects/cd/${manifestHash.slice(2)}`,
                    deduplicated: false,
                }
                if (
                    command === 'asset_cas_job_release'
                    || command === 'native_file_job_forget'
                ) return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        await expect(receipt.prepareOwnerManifestAndSeal(Uint8Array.of(1, 2, 3)))
            .resolves.toMatchObject({ contentHash: manifestHash, byteSize: 3 })
        await receipt.confirmActivated()

        expect(calls.slice(2)).toEqual([
            ['asset_cas_job_finalize_content', {
                sessionId: 'content-1',
                ownerManifest: [1, 2, 3],
            }],
            ['asset_cas_job_release', { sessionId: 'content-1', outcome: 'committed' }],
            ['native_file_job_forget', { jobId: 'content-1' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('contentAssets')
    })

    it('rejects a mismatched CAS session and aborts the actual job session before forgetting', async () => {
        const calls: string[] = []

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', {
                        ...preparedContent,
                        casSessionId: 'different-session',
                    })
                }
                if (command === 'asset_cas_job_release' || command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow(/casSessionId must match/i)

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('aborts an unsealed session once and makes later confirmation a no-op', async () => {
        const calls: string[] = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_release' || command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        await receipt.cancel()
        await receipt.confirmActivated()
        await receipt.cancel()

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('aborts and forgets when the finalizer rejects before upsert begins', async () => {
        const calls: string[] = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_finalize_content') throw new Error('response lost')
                if (command === 'asset_cas_job_release') return null
                if (command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        await expect(receipt.prepareOwnerManifestAndSeal(Uint8Array.of(1)))
            .rejects.toThrow('response lost')

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_finalize_content',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('serializes cancellation behind an in-flight finalizer and prevents activation', async () => {
        const calls: string[] = []
        const manifestHash = 'cd'.repeat(32)
        let resolveFinalize!: (value: unknown) => void
        const pendingFinalize = new Promise((resolve) => {
            resolveFinalize = resolve
        })
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_finalize_content') return pendingFinalize
                if (command === 'asset_cas_job_release' || command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        const activation = receipt.prepareOwnerManifestAndSeal(Uint8Array.of(1))
        const cancellation = receipt.cancel()
        await Promise.resolve()
        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_finalize_content',
        ])

        resolveFinalize({
            contentHash: manifestHash,
            byteSize: 1,
            physicalKey: `assets-v2/objects/cd/${manifestHash.slice(2)}`,
            deduplicated: false,
        })

        await expect(activation).rejects.toMatchObject({ name: 'AbortError' })
        await expect(cancellation).resolves.toBeUndefined()
        await expect(receipt.confirmActivated()).resolves.toBeUndefined()
        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_finalize_content',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('rejects a concurrent finalizer call before sending a second IPC', async () => {
        const manifestHash = 'cd'.repeat(32)
        let resolveFinalize!: (value: unknown) => void
        const pendingFinalize = new Promise((resolve) => {
            resolveFinalize = resolve
        })
        const finalizeCalls: string[] = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_finalize_content') {
                    finalizeCalls.push(command)
                    return pendingFinalize
                }
                if (command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        const first = receipt.prepareOwnerManifestAndSeal(Uint8Array.of(1))
        await expect(receipt.prepareOwnerManifestAndSeal(Uint8Array.of(2)))
            .rejects.toThrow(/only be finalized once/i)
        resolveFinalize({
            contentHash: manifestHash,
            byteSize: 1,
            physicalKey: `assets-v2/objects/cd/${manifestHash.slice(2)}`,
            deduplicated: false,
        })
        await first
        await receipt.cancel()

        expect(finalizeCalls).toEqual(['asset_cas_job_finalize_content'])
    })

    it('attempts native job forget when committed release fails', async () => {
        const calls: string[] = []
        const manifestHash = 'cd'.repeat(32)
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_finalize_content') return {
                    contentHash: manifestHash,
                    byteSize: 1,
                    physicalKey: `assets-v2/objects/cd/${manifestHash.slice(2)}`,
                    deduplicated: false,
                }
                if (command === 'asset_cas_job_release') throw new Error('release response lost')
                if (command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        await receipt.prepareOwnerManifestAndSeal(Uint8Array.of(1))
        await expect(receipt.confirmActivated()).rejects.toThrow('release response lost')

        expect(calls.slice(-2)).toEqual(['asset_cas_job_release', 'native_file_job_forget'])
    })

    it('does not release a missing CAS journal after a terminal native failure', async () => {
        const calls: string[] = []

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    const status = contentStatus('failed', 'complete')
                    status.error = { code: 'bad-card', message: 'bad card' }
                    return status
                }
                if (command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow('bad card')

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('forgets validated staging even when pre-finalize abort release fails', async () => {
        const calls: string[] = []

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', {
                        ...preparedContent,
                        casSessionId: 'wrong',
                    })
                }
                if (command === 'asset_cas_job_release') throw new Error('release response lost')
                if (command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow(/casSessionId must match/i)

        expect(calls.slice(-2)).toEqual(['asset_cas_job_release', 'native_file_job_forget'])
    })

    it('retains the finalized journal but forgets the native job when database activation is ambiguous', async () => {
        const calls: string[] = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_finalize_content') return {
                    contentHash: 'cd'.repeat(32),
                    byteSize: 1,
                    physicalKey: `assets-v2/objects/cd/${'cd'.repeat(32).slice(2)}`,
                    deduplicated: false,
                }
                if (command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        await expect(runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async (value) => value),
                activate: vi.fn(async (_content, lifecycle) => {
                    await lifecycle.prepareOwnerManifestAndSeal(Uint8Array.of(1))
                    throw new Error('upsert response lost')
                }),
            },
        )).rejects.toThrow('upsert response lost')

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_finalize_content',
            'native_file_job_forget',
        ])
    })

    it('aborts a declined mapping and makes confirmation remain a no-op', async () => {
        const receipt = {
            jobId: 'content-1',
            content: preparedContent,
            warningCodes: [],
            prepareOwnerManifestAndSeal: vi.fn(),
            confirmActivated: vi.fn(async () => undefined),
            cancel: vi.fn(async () => undefined),
        }

        await expect(runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async (value) => value),
                activate: vi.fn(async () => null),
            },
        )).resolves.toBeNull()

        expect(receipt.cancel).toHaveBeenCalledOnce()
        expect(receipt.confirmActivated).not.toHaveBeenCalled()
    })

    it('accepts an empty optional asset display name', async () => {
        const unnamedContent = {
            ...preparedContent,
            assets: [{ ...preparedContent.assets[0], name: '' }],
        }
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', unnamedContent)
                }
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        expect(receipt.content.assets[0].name).toBe('')
    })

    it('accepts CharX descriptors with a member portrait, module arrays, and duplicate logical IDs', async () => {
        const charxContent = {
            ...preparedContent,
            format: 'appended-charx-jpeg',
            portraitLogicalId: `assets/${'ab'.repeat(32)}.cas-portrait`,
            module: { trigger: [], regex: [], lorebook: [] },
            assets: [
                {
                    ...preparedContent.assets[0],
                    logicalId: `assets/${'ab'.repeat(32)}.cas-portrait`,
                },
                {
                    ...preparedContent.assets[0],
                    referenceKey: 'data.assets.1.uri',
                    token: 'staged-token-2',
                    logicalId: `assets/${'ab'.repeat(32)}.cas-portrait`,
                },
            ],
        }

        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.jpg' },
            'card.jpg',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', charxContent as PreparedNativeContent)
                }
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        expect(receipt.content).toEqual(charxContent)
    })

    it('accepts the bounded PNG metadata wrapper, ordinary portrait, and opaque chunk aliases', async () => {
        const portraitHash = '31'.repeat(32)
        const chunkHash = '32'.repeat(32)
        const pngContent = {
            casSessionId: 'content-1',
            format: 'png-card',
            metadata: {
                chara: Buffer.from('{"name":"old"}').toString('base64'),
                ccv3: Buffer.from('{"name":"new"}').toString('base64'),
            },
            portraitLogicalId: `assets/${portraitHash}.png`,
            assets: [
                {
                    referenceKey: 'native-png-portrait',
                    token: 'native-png-portrait',
                    logicalId: `assets/${portraitHash}.png`,
                    objectHash: portraitHash,
                    byteSize: 25,
                    mime: 'image/png',
                    name: `${portraitHash}.png`,
                    ext: 'png',
                },
                {
                    referenceKey: '007',
                    token: '007',
                    logicalId: `assets/${chunkHash}.png`,
                    objectHash: chunkHash,
                    byteSize: 4,
                    mime: '',
                    name: `${chunkHash}.png`,
                    ext: 'png',
                },
            ],
        } as PreparedNativeContent

        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.png' },
            'card.png',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', pngContent)
                }
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        expect(receipt.content).toEqual(pngContent)
        expect(receipt.content.assets[1].mime).toBe('')
        expect(JSON.stringify(receipt.content)).not.toMatch(/staged|payload|path/i)
    })

    const invalidPngContentCases: Array<[
        string,
        Record<string, unknown>,
        string,
    ]> = [
        ['missing portrait', { portraitLogicalId: undefined }, 'portraitLogicalId is required'],
        ['portrait after a chunk', { assets: undefined }, 'portrait must be the first asset'],
        ['nonempty chunk MIME', { chunkMime: 'image/png' }, 'mime must be empty'],
        ['renamed chunk token', { chunkToken: 'different' }, 'token must equal referenceKey'],
        ['oversized selected metadata', { ccv3: 'x'.repeat(MAX_CONTENT_METADATA_BYTES + 1) }, 'metadata exceeds'],
    ]

    it.each(invalidPngContentCases)('rejects PNG content with %s', async (_case, change, expectedMessage) => {
        const portraitHash = '41'.repeat(32)
        const chunkHash = '42'.repeat(32)
        const portrait = {
            referenceKey: 'native-png-portrait',
            token: 'native-png-portrait',
            logicalId: `assets/${portraitHash}.png`,
            objectHash: portraitHash,
            byteSize: 25,
            mime: 'image/png',
            name: `${portraitHash}.png`,
            ext: 'png',
        }
        const chunk = {
            referenceKey: '007',
            token: typeof change.chunkToken === 'string' ? change.chunkToken : '007',
            logicalId: `assets/${chunkHash}.png`,
            objectHash: chunkHash,
            byteSize: 4,
            mime: typeof change.chunkMime === 'string' ? change.chunkMime : '',
            name: `${chunkHash}.png`,
            ext: 'png',
        }
        const invalidContent = {
            casSessionId: 'content-1',
            format: 'png-card',
            metadata: {
                ccv3: typeof change.ccv3 === 'string'
                    ? change.ccv3
                    : Buffer.from('{}').toString('base64'),
            },
            ...(change.portraitLogicalId === undefined && Object.hasOwn(change, 'portraitLogicalId')
                ? {}
                : { portraitLogicalId: `assets/${portraitHash}.png` }),
            assets: change.assets === undefined && Object.hasOwn(change, 'assets')
                ? [chunk, portrait]
                : [portrait, chunk],
        }

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.png' },
            'card.png',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', invalidContent as PreparedNativeContent)
                }
                if (command === 'asset_cas_job_release' || command === 'native_file_job_forget') return null
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow(expectedMessage)
    })

    it.each([
        ['non-member portrait', { portraitLogicalId: `assets/${'cd'.repeat(32)}.portrait` }, 'portraitLogicalId must reference a prepared asset'],
        ['non-array module field', { module: { trigger: {} } }, 'module trigger must be an array'],
        ['unsafe logical suffix', { assets: [{ ...preparedContent.assets[0], logicalId: `assets/${'ab'.repeat(32)}../png` }] }, 'logicalId suffix is invalid'],
    ])('rejects invalid CharX %s', async (_case, changes, expectedMessage) => {
        const invalidContent = {
            ...preparedContent,
            format: 'charx-card',
            ...changes,
        }

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.charx' },
            'card.charx',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', invalidContent as PreparedNativeContent)
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow(expectedMessage)
    })

    it('forgets retained staging only after activation is confirmed and the job is terminal', async () => {
        const calls: string[] = []
        const receipt = await prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_finalize_content') return {
                    contentHash: 'cd'.repeat(32),
                    byteSize: 1,
                    physicalKey: `assets-v2/objects/cd/${'cd'.repeat(32).slice(2)}`,
                    deduplicated: false,
                }
                if (command === 'asset_cas_job_release') return true
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        expect(calls).toEqual(['native_file_job_start', 'native_file_job_status'])
        await receipt.prepareOwnerManifestAndSeal(Uint8Array.of(1))
        await receipt.confirmActivated()
        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_finalize_content',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('forgets a retained success when status reporting throws before returning the receipt', async () => {
        const calls: string[] = []

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {
                onStatus: () => { throw new Error('status callback failed') },
            },
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_release') return true
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow('status callback failed')

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('cancels, drains, and forgets when aborted before content is prepared', async () => {
        const controller = new AbortController()
        const calls: string[] = []
        let statusCount = 0

        const result = prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            { signal: controller.signal },
            nativeDependencies(
                async (command) => {
                    calls.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'content-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? contentStatus('running', 'reading-source')
                            : contentStatus('cancelled', 'complete')
                    }
                    if (command === 'native_file_job_cancel') return 'requested'
                    if (command === 'asset_cas_job_release') return true
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                async () => controller.abort(),
            ),
        )

        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('aborts the unsealed CAS session when a running cancellation drains to success', async () => {
        const controller = new AbortController()
        const calls: string[] = []
        let statusCount = 0

        const result = prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            { signal: controller.signal },
            nativeDependencies(
                async (command) => {
                    calls.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'content-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? contentStatus('running', 'reading-source')
                            : contentStatus('succeeded', 'complete', preparedContent)
                    }
                    if (command === 'native_file_job_cancel') return 'too-late'
                    if (command === 'asset_cas_job_release') return null
                    if (command === 'native_file_job_forget') return null
                    throw new Error(`Unexpected command: ${command}`)
                },
                async () => controller.abort(),
            ),
        )

        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('returns AbortError when cancellation races with terminal success', async () => {
        const controller = new AbortController()
        const calls: string[] = []

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            { signal: controller.signal },
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    controller.abort()
                    return contentStatus('succeeded', 'complete', preparedContent)
                }
                if (command === 'asset_cas_job_release') return true
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toMatchObject({ name: 'AbortError' })

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'asset_cas_job_release',
            'native_file_job_forget',
        ])
    })

    it('rejects malformed logical asset descriptors and cleans native staging', async () => {
        const calls: string[] = []
        const malformed = {
            ...preparedContent,
            assets: [{ ...preparedContent.assets[0], objectHash: 'not-a-sha256' }],
        }

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus(
                        'succeeded',
                        'complete',
                        malformed as PreparedNativeContent,
                    )
                }
                if (command === 'asset_cas_job_release') return true
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toMatchObject({ code: 'invalid-prepared-content' })
        expect(calls.slice(-2)).toEqual(['asset_cas_job_release', 'native_file_job_forget'])
    })

    it.each([
        [
            'unsafe extension',
            [{ ...preparedContent.assets[0], ext: '../png' }],
            'ext is invalid',
        ],
        [
            'uncorrelated logical ID',
            [{ ...preparedContent.assets[0], logicalId: 'assets/wrong.png' }],
            'logicalId does not match',
        ],
        [
            'empty MIME outside PNG',
            [{ ...preparedContent.assets[0], mime: '' }],
            'mime must be a nonempty string',
        ],
        [
            'duplicate token',
            [
                preparedContent.assets[0],
                {
                    ...preparedContent.assets[0],
                    referenceKey: 'data.assets.1.uri',
                    logicalId: `assets/${'cd'.repeat(32)}.png`,
                    objectHash: 'cd'.repeat(32),
                },
            ],
            'token is duplicated',
        ],
    ])('rejects %s in logical descriptors', async (_case, assets, expectedMessage) => {
        const invalidContent = { ...preparedContent, assets }
        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') {
                    return contentStatus('succeeded', 'complete', invalidContent)
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow(expectedMessage)
    })

    it.each([
        ['failed status', 'failed' as const, preparedContent, 'content failed'],
        ['cancelled status', 'cancelled' as const, preparedContent, 'Native file job was cancelled'],
        [
            'malformed descriptor',
            'succeeded' as const,
            { ...preparedContent, assets: [{ ...preparedContent.assets[0], objectHash: 'bad' }] },
            'objectHash is invalid',
        ],
    ])('preserves the original %s error when terminal cleanup fails', async (
        _case,
        state,
        content,
        expectedMessage,
    ) => {
        const terminal = contentStatus(state, 'complete', content as PreparedNativeContent)
        if (state === 'failed') terminal.error = { code: 'content-failed', message: 'content failed' }

        await expect(prepareNativeContentImport(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {},
            nativeDependencies(async (command) => {
                if (command === 'native_file_job_start') return { jobId: 'content-1' }
                if (command === 'native_file_job_status') return terminal
                if (command === 'native_file_job_forget') throw new Error('cleanup failed')
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow(expectedMessage)
    })

    it('maps and atomically activates through an injected route before acknowledging staging', async () => {
        const events: string[] = []
        const receipt = {
            jobId: 'content-1',
            content: preparedContent,
            warningCodes: [],
            prepareOwnerManifestAndSeal: vi.fn(),
            confirmActivated: vi.fn(async () => { events.push('confirmed') }),
            cancel: vi.fn(async () => { events.push('cancelled') }),
        }

        const result = await runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async (content) => {
                    events.push('mapped')
                    return { character: content.metadata }
                }),
                activate: vi.fn(async () => {
                    events.push('activated')
                    return { characterId: 'character-1' }
                }),
            },
        )

        expect(result).toEqual({ characterId: 'character-1' })
        expect(events).toEqual(['mapped', 'activated', 'confirmed'])
        expect(receipt.cancel).not.toHaveBeenCalled()
    })

    it('keeps committed activation successful when terminal acknowledgement fails', async () => {
        const cleanupWarnings: unknown[] = []
        const receipt = {
            jobId: 'content-1',
            content: preparedContent,
            warningCodes: [],
            prepareOwnerManifestAndSeal: vi.fn(),
            confirmActivated: vi.fn(async () => { throw new Error('forget failed') }),
            cancel: vi.fn(async () => undefined),
        }

        await expect(runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async () => ({ character: preparedContent.metadata })),
                activate: vi.fn(async () => ({ characterId: 'character-1' })),
                onCleanupWarning: (error) => cleanupWarnings.push(error),
            },
        )).resolves.toEqual({ characterId: 'character-1' })

        expect(cleanupWarnings).toEqual([expect.objectContaining({ message: 'forget failed' })])
        expect(receipt.cancel).not.toHaveBeenCalled()
    })

    it('keeps committed activation successful when cleanup warning reporting throws', async () => {
        const receipt = {
            jobId: 'content-1',
            content: preparedContent,
            warningCodes: [],
            prepareOwnerManifestAndSeal: vi.fn(),
            confirmActivated: vi.fn(async () => { throw new Error('forget failed') }),
            cancel: vi.fn(async () => undefined),
        }

        await expect(runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async () => ({ character: preparedContent.metadata })),
                activate: vi.fn(async () => ({ characterId: 'character-1' })),
                onCleanupWarning: () => { throw new Error('warning handler failed') },
            },
        )).resolves.toEqual({ characterId: 'character-1' })

        expect(receipt.cancel).not.toHaveBeenCalled()
    })

    it('cancels retained staging when mapping or activation fails', async () => {
        const receipt = {
            jobId: 'content-1',
            content: preparedContent,
            warningCodes: [],
            prepareOwnerManifestAndSeal: vi.fn(),
            confirmActivated: vi.fn(async () => undefined),
            cancel: vi.fn(async () => undefined),
        }

        await expect(runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.json' },
            'card.json',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async () => ({ character: preparedContent.metadata })),
                activate: vi.fn(async () => { throw new Error('revision conflict') }),
            },
        )).rejects.toThrow('revision conflict')

        expect(receipt.cancel).toHaveBeenCalledOnce()
        expect(receipt.confirmActivated).not.toHaveBeenCalled()
    })
})
