import { describe, expect, it, vi } from 'vitest'

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('../platform', () => ({ isTauriAndroid: false, isTauriDesktop: false }))
vi.mock('./persistentDataRuntime.svelte', () => ({ getPersistentDataRuntime: vi.fn() }))
vi.mock('./database.svelte', () => ({ getDatabase: vi.fn() }))

import { exportNativeModuleRisumFromPicker } from './nativeModuleRisumExportRoute'

describe('native RISUM export route', () => {
    it('derives the exact root index by object identity after flush', async () => {
        const selected = { id: 'duplicate', name: 'Selected' }
        const modules = [{ id: 'duplicate', name: 'Other' }, selected]
        const calls: unknown[] = []
        await exportNativeModuleRisumFromPicker(selected as never, {}, {
            isDesktop: () => true,
            isAndroid: () => false,
            chooseDestination: async () => 'C:\\chosen\\Selected.risum',
            runtime: () => ({
                revision: 12,
                flushPendingData: async (reason) => { calls.push(['flush', reason]) },
            }),
            modules: () => modules as never,
            runExport: async (input) => {
                calls.push(['export', input])
                return {
                    revision: 12,
                    sourceBytes: 7,
                    sourceSha256: 'a'.repeat(64),
                    characterCount: 0,
                    presetCount: 0,
                    warningCodes: [],
                }
            },
        })
        expect(calls).toEqual([
            ['flush', 'native-risum-export'],
            ['export', {
                moduleIndex: 1,
                expectedRevision: 12,
                destination: { type: 'desktopPath', path: 'C:\\chosen\\Selected.risum' },
            }],
        ])
    })

    it('keeps Web on the legacy path and rejects a detached equal-ID object', async () => {
        const module = { id: 'duplicate', name: 'Selected' }
        const base = {
            isAndroid: () => false,
            chooseDestination: async () => 'unused',
            runtime: () => ({ revision: 1, flushPendingData: async () => undefined }),
            modules: () => [module] as never,
            runExport: async () => { throw new Error('must not export') },
        }
        await expect(exportNativeModuleRisumFromPicker(module as never, {}, {
            ...base,
            isDesktop: () => false,
        })).resolves.toBeUndefined()
        await expect(exportNativeModuleRisumFromPicker({ ...module } as never, {}, {
            ...base,
            isDesktop: () => true,
        })).rejects.toThrow('exact root module')
    })
})
