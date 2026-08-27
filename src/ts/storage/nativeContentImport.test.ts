import { describe, expect, it, vi } from 'vitest'

import {
    prepareNativeContentImport,
    type NativeFileJobDependencies,
    type NativeFileJobStatus,
    type PreparedNativeContent,
} from './nativeFileJobs'
import { runNativePreparedContentRoute } from './nativePreparedContentRoute'

const preparedContent: PreparedNativeContent = {
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
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )

        expect(calls).toEqual(['native_file_job_start', 'native_file_job_status'])
        await receipt.confirmActivated()
        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
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
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toThrow('status callback failed')

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
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
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toMatchObject({ name: 'AbortError' })

        expect(calls).toEqual([
            'native_file_job_start',
            'native_file_job_status',
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
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
        )).rejects.toMatchObject({ code: 'invalid-prepared-content' })
        expect(calls.at(-1)).toBe('native_file_job_forget')
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
