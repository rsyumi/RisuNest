// @vitest-environment happy-dom
import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

vi.mock('src/ts/platform', async (importOriginal) => ({
    ...(await importOriginal<typeof import('src/ts/platform')>()),
    isTauri: false,
    isTauriAndroid: false,
    isTauriDesktop: false,
}))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: { account: undefined } } }))
vi.mock('src/ts/alert', () => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
    alertSelect: vi.fn(),
}))
vi.mock('src/ts/drive/backuplocal', () => ({ LoadLocalBackup: vi.fn() }))
vi.mock('src/ts/storage/sync/syncConflictRestore', () => ({ openSyncConflictBackups: vi.fn() }))
vi.mock('src/ts/storage/nativePersistentMaintenance', () => ({
    restoreNativePersistentSnapshot: vi.fn(),
    restartNativeApp: vi.fn(),
}))
vi.mock('src/ts/storage/sync/nativeOfficialAccountFlow', () => ({
    getNativeOfficialAccountFlow: vi.fn(),
}))
vi.mock('src/ts/storage/risuSaveFileRouteProduction.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        nativeFileOperation: writable({
            kind: 'export',
            presentation: 'inline',
            status: { stage: 'writing', completedUnits: 1, totalUnits: 2 },
        }),
        importRisuSaveFromSystemPicker: vi.fn(),
        exportRisuSaveFromSystemPicker: vi.fn(),
    }
})
vi.mock('src/ts/storage/portableBackupFileRouteProduction.svelte', () => ({
    restoreBackupFromSystemPicker: vi.fn(),
}))
vi.mock('src/ts/storage/risuSaveFileRoute', () => ({
    alertPartialDestinationWarning: vi.fn(),
    hasPartialDestinationWarning: vi.fn(() => false),
}))
vi.mock('src/ts/storage/nativeFileJobManager', () => ({
    cancelActiveNativeFileOperation: vi.fn(),
    dismissNativeFileOperationOutcome: vi.fn(),
    nativeFileOperationOutcomeShown: vi.fn(() => false),
}))
vi.mock('src/ts/gui/nativeFileJobProgress', () => ({
    nativeFileJobProgressText: vi.fn(() => 'Writing backup'),
}))
vi.mock('src/ts/storage/sync/external/bridge', () => ({
    getExternalStorageBridge: () => ({
        getState: vi.fn(async () => ({
            supported: false,
            selection: {
                kind: 'none',
                selectionEpoch: 'test-selection',
                paused: false,
                decisionRequired: false,
            },
            connections: [],
            jobs: [],
        })),
    }),
}))
vi.mock('src/ts/storage/sync/external/production', () => ({
    refreshExternalStorageProductionState: vi.fn(),
    requestExternalStorageNow: vi.fn(),
    requestExternalStorageRestore: vi.fn(),
}))

import RisuNestBackupRestore from './RisuNestBackupRestore.svelte'

let mounted: ReturnType<typeof mount> | undefined

afterEach(async () => {
    if (mounted) await unmount(mounted)
    mounted = undefined
    document.body.replaceChildren()
})

it('announces inline backup progress through a polite status region', async () => {
    const target = document.createElement('div')
    document.body.append(target)
    mounted = mount(RisuNestBackupRestore, { target })
    await tick()

    const status = target.querySelector('[role="status"]')
    expect(status?.getAttribute('aria-live')).toBe('polite')
    expect(status?.textContent).toContain('Writing backup')
})
