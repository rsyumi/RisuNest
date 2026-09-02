// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import type { DeviceSyncControllerSnapshot } from 'src/ts/storage/sync/deviceSyncController'
import { encodeLogicalRecordKey } from 'src/ts/storage/sync/logicalRecordKey'

const environment = vi.hoisted(() => ({ android: false, notifications: true }))
const settingsState = vi.hoisted(() => ({
    value: { syncListenMethod: 'lan', syncFixedPort: 32145, syncPublicBaseUrl: '', syncAutoListen: false },
}))
const alerts = vi.hoisted(() => ({ alertConfirm: vi.fn(async () => true) }))
const database = vi.hoisted(() => ({
    characters: [{ chaId: 'char-a', name: 'Aster', chats: [{ id: 'chat-a', name: 'First meeting' }] }],
}))
const qrCode = vi.hoisted(() => ({ toDataURL: vi.fn<(uri: string) => Promise<string>>() }))
const deepLinks = vi.hoisted(() => {
    let listener: ((uri: string) => void) | undefined
    const unsubscribe = vi.fn(() => { listener = undefined })
    return {
        subscribe: vi.fn((next: (uri: string) => void) => { listener = next; return unsubscribe }),
        unsubscribe,
        emit: (uri: string) => listener?.(uri),
    }
})
const controllerState = vi.hoisted(() => {
    let listener: ((snapshot: unknown) => void) | undefined
    const controller = {
        snapshot: vi.fn(), subscribe: vi.fn((next: (snapshot: unknown) => void) => { listener = next; return () => { listener = undefined } }),
        initialize: vi.fn(async () => undefined), prepare: vi.fn(async () => undefined), start: vi.fn(async () => undefined), stop: vi.fn(async () => undefined), rotateLink: vi.fn(async () => undefined),
        revokeIncoming: vi.fn(async () => undefined), revokeOutgoing: vi.fn(async () => undefined), stageLink: vi.fn(), claimStagedClone: vi.fn(async () => undefined), selectRegisteredClone: vi.fn(async () => undefined),
        confirmCloneReplace: vi.fn(async () => undefined), downloadClone: vi.fn(async () => undefined), resumeClone: vi.fn(async () => undefined), cancelClone: vi.fn(async () => undefined),
        pullStagedDelta: vi.fn(async () => undefined), pullRegisteredDelta: vi.fn(async () => undefined), syncStagedBidirectional: vi.fn(async () => undefined), syncRegisteredBidirectional: vi.fn(async () => undefined),
        resolveRegisteredBidirectional: vi.fn(async () => undefined), resumeBidirectional: vi.fn(async () => undefined), acknowledgeBidirectional: vi.fn(async () => undefined), abandonBidirectional: vi.fn(async () => undefined),
    }
    return { controller, emit: (snapshot: unknown) => listener?.(snapshot) }
})

vi.mock('src/ts/platform', () => ({ get isTauriAndroid() { return environment.android } }))
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/storage/deviceSettings', () => ({
    getDeviceSettings: () => ({ ...settingsState.value }),
    updateDeviceSettings: (partial: Record<string, unknown>) => Object.assign(settingsState.value, partial),
}))
vi.mock('src/ts/storage/sync/deviceSyncProduction', () => ({ getProductionDeviceSyncController: () => controllerState.controller }))
vi.mock('src/ts/storage/sync/peerCloneDeepLink', () => ({ subscribeDeviceSyncUri: deepLinks.subscribe }))
vi.mock('src/ts/storage/sync/peerSyncShared', () => ({ androidPeerSyncNotificationsEnabled: () => environment.notifications }))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => database }))
vi.mock('src/lang', async () => {
    const language = (await import('src/lang/en')).languageEnglish
    return { language: { ...language, risuNest: { ...language.risuNest, sync: { ...language.risuNest.sync, menuTitle: '기기 동기화' } } } }
})
vi.mock('qrcode', () => ({ default: { toDataURL: qrCode.toDataURL } }))

import DeviceSyncSettings from './DeviceSyncSettings.svelte'

const cloneBase = {
    platform: 'desktop' as const, resumeAvailable: false,
    sourceStatus: { phase: 'idle' as const, devices: [] }, tunnelStatus: { phase: 'idle' as const }, sourcePairingUri: '', targetPhase: 'idle' as const, error: null, warning: null,
    state: { source: { phase: 'idle' as const, revokedDeviceIds: [] }, target: { phase: 'idle' as const, destructiveConfirmed: false, completedBytes: 0 } },
}
const deltaBase = { sourceStatus: { phase: 'idle' as const, devices: [] }, tunnelStatus: { phase: 'idle' as const }, sourcePairingUri: '', pullPhase: 'idle' as const, error: null }
const bidiBase = { sourceStatus: { phase: 'idle' as const, devices: [] }, sourcePairingUri: '', operationPhase: 'idle' as const, operationRetained: false, sourceBusy: false, sourceError: null, operationError: null }

function snapshot(partial: Partial<DeviceSyncControllerSnapshot> = {}): DeviceSyncControllerSnapshot {
    return { source: { phase: 'idle' }, sources: [], devices: [], error: null, stagedLink: null, stagedSourceDeviceId: null, activeCloneSourceDeviceId: null, activeBidirectionalSourceDeviceId: null, expiredSourceIds: [], targets: { clone: cloneBase, delta: deltaBase, bidirectional: bidiBase }, ...partial }
}

describe('DeviceSyncSettings', () => {
    let mounted: ReturnType<typeof mount> | undefined
    let target: HTMLElement

    beforeEach(() => {
        environment.android = false; environment.notifications = true
        settingsState.value = { syncListenMethod: 'lan', syncFixedPort: 32145, syncPublicBaseUrl: '', syncAutoListen: false }
        controllerState.controller.snapshot.mockReturnValue(snapshot())
        vi.clearAllMocks()
        qrCode.toDataURL.mockImplementation(async (uri: string) => `data:image/mock,${uri}`)
        target = document.createElement('div'); document.body.append(target)
    })
    afterEach(async () => { if (mounted) await unmount(mounted); mounted = undefined; document.body.replaceChildren(); vi.useRealTimers() })
    const render = async (state = snapshot()) => { controllerState.controller.snapshot.mockReturnValue(state); mounted = mount(DeviceSyncSettings, { target }); await tick() }
    const button = (name: string) => [...target.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.textContent?.trim() === name)

    it('renders the exact three-card order and desktop sharing controls accessibly', async () => {
        await render()
        expect([...target.querySelectorAll('[data-sync-card]')].map((node) => node.getAttribute('data-sync-card'))).toEqual(['sharing', 'devices', 'work'])
        expect(target.querySelector('[role="radiogroup"]')).not.toBeNull()
        expect(target.querySelectorAll('[role="radio"]')).toHaveLength(3)
        const permissionPanel = target.querySelector('[data-permissions]')!
        const start = button('Start sharing')!
        expect(permissionPanel.compareDocumentPosition(start) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
        expect(target.textContent).toContain('LAN connections are not encrypted')
    })

    it('persists method-specific fields without exposing a tunnel token', async () => {
        await render()
        button('Temporary internet address')!.click(); await tick()
        expect(settingsState.value.syncListenMethod).toBe('quick')
        expect(target.querySelector('#device-sync-port')).toBeNull()
        expect(target.textContent).toContain('address changes when you restart')
        button('Fixed address (advanced)')!.click(); await tick()
        expect(target.querySelector('#device-sync-port')).not.toBeNull()
        expect(target.querySelector('#device-sync-public-url')).not.toBeNull()
        expect(target.textContent).toContain('How to connect it yourself')
        expect([...target.querySelectorAll('input')].some((input) => `${input.id} ${input.getAttribute('name') ?? ''}`.includes('token'))).toBe(false)
    })

    it('renders Android warning before sharing and keeps LAN-only copy inside the sharing card', async () => {
        environment.android = true; environment.notifications = false
        await render()
        const intro = target.querySelector('[data-sync-intro]')!, warning = target.querySelector('[data-notification-warning]')!, sharing = target.querySelector('[data-sync-card="sharing"]')!
        expect(intro.compareDocumentPosition(warning) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
        expect(warning.compareDocumentPosition(sharing) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
        expect(sharing.textContent).toContain('Android supports same-network')
        expect(target.querySelector('[role="radiogroup"]')).toBeNull()
    })

    it('starts sharing in prepare then start order and disables the control during transition', async () => {
        const calls: string[] = []; let release!: () => void
        controllerState.controller.prepare.mockImplementationOnce(() => { calls.push('prepare'); return new Promise<void>((resolve) => { release = resolve }) })
        controllerState.controller.start.mockImplementationOnce(async () => { calls.push('start') })
        await render(); button('Start sharing')!.click(); await tick()
        expect(button('Start sharing')?.disabled).toBe(true)
        expect(controllerState.controller.prepare).toHaveBeenCalledWith({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })
        release(); await vi.waitFor(() => expect(controllerState.controller.start).toHaveBeenCalledWith({ read: true, bidirectional: false }))
        expect(calls).toEqual(['prepare', 'start'])
    })

    it('stops a running share without preparing another session', async () => {
        await render(snapshot({ source: { phase: 'running' } }))
        button('Stop sharing')!.click()
        await vi.waitFor(() => expect(controllerState.controller.stop).toHaveBeenCalledOnce())
        expect(controllerState.controller.prepare).not.toHaveBeenCalled()
        expect(controllerState.controller.start).not.toHaveBeenCalled()
    })

    it('renders the latest safe page action error without exposing raw details', async () => {
        controllerState.controller.stop.mockRejectedValueOnce(new Error('raw native secret'))
        await render(snapshot({ source: { phase: 'running' } }))
        button('Stop sharing')!.click()
        await vi.waitFor(() => expect(target.querySelector('[data-share-error]')?.textContent).toBe('Error'))
        expect(target.querySelector('[data-sync-card="sharing"]')?.contains(target.querySelector('[data-share-error]'))).toBe(true)
        expect(target.textContent).not.toContain('raw native secret')
    })

    it('preserves a concrete safe category from a rejected sharing action', async () => {
        controllerState.controller.prepare.mockRejectedValueOnce('port-unavailable')
        await render()

        button('Start sharing')!.click()

        await vi.waitFor(() => expect(target.querySelector('[data-share-error]')?.textContent)
            .toBe('That port is already in use. Choose another port.'))
    })

    it('keeps a receive action error inside the work card', async () => {
        controllerState.controller.pullStagedDelta.mockImplementationOnce(async () => {
            controllerState.emit(snapshot({ error: 'operation-failed' }))
            throw new Error('private target detail')
        })
        await render()

        button('Get changes only')!.click()

        await vi.waitFor(() => expect(target.querySelector('[data-work-error]')?.textContent).toBe('Error'))
        expect(target.querySelector('[data-sync-card="work"]')?.contains(target.querySelector('[data-work-error]'))).toBe(true)
        expect(target.querySelector('[data-sync-card="sharing"]')?.querySelector('[role="alert"]')).toBeNull()
        expect(target.textContent).not.toContain('private target detail')
    })

    it('keeps a source action error in sharing while terminal work remains visible', async () => {
        controllerState.controller.stop.mockRejectedValueOnce('port-unavailable')
        await render(snapshot({
            source: { phase: 'running' },
            targets: {
                clone: cloneBase,
                delta: { ...deltaBase, pullPhase: 'completed', pullResult: { kind: 'noChanges', revision: 1, transferredObjects: 0, transferredBytes: 0 } },
                bidirectional: bidiBase,
            },
        }))

        button('Stop sharing')!.click()

        await vi.waitFor(() => expect(target.querySelector('[data-share-error]')?.textContent)
            .toBe('That port is already in use. Choose another port.'))
        expect(target.querySelector('[data-work-error]')).toBeNull()
    })

    it('selects only incoming targets without contacting them and revokes each direction separately', async () => {
        await render(snapshot({
            devices: [{ deviceId: 'out-a', name: 'Outgoing', permissions: ['read', 'bidirectional'], totalBytes: 2048 }],
            sources: [{ deviceId: 'in-a', name: 'Incoming', permissions: ['read'], totalBytes: 1024 }],
        }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        expect([...select.options].map((option) => option.text)).toEqual(['Incoming', 'Use a new registration link...'])
        select.value = 'in-a'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()
        expect(controllerState.controller.selectRegisteredClone).not.toHaveBeenCalled()
        const removeButtons = [...target.querySelectorAll<HTMLButtonElement>('button')].filter((candidate) => candidate.textContent?.trim() === 'Remove')
        removeButtons[0].click(); await vi.waitFor(() => expect(controllerState.controller.revokeOutgoing).toHaveBeenCalledWith('out-a'))
        removeButtons[1].click(); await vi.waitFor(() => expect(controllerState.controller.revokeIncoming).toHaveBeenCalledWith('in-a'))
        expect(target.textContent).toContain('2.0 KiB total')
        expect(target.querySelectorAll('[data-device-icon]')).toHaveLength(2)
    })

    it('switches to a newly received registration link and fills the mounted input', async () => {
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F127.0.0.1%3A32145&session=00000000-0000-4000-8000-000000000001&manifest=' + 'a'.repeat(64) + '#claim=' + 'b'.repeat(64)
        await render(snapshot({ sources: [{ deviceId: 'source-a', name: 'Source A', permissions: ['read'] }] }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'source-a'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()

        deepLinks.emit(uri)
        await tick()

        expect(select.value).toBe('new-link')
        expect(target.querySelector<HTMLInputElement>('#device-sync-link')?.value).toBe(uri)
        await unmount(mounted!); mounted = undefined
        expect(deepLinks.unsubscribe).toHaveBeenCalledOnce()
    })

    it('enforces the selected incoming permissions and blocks an expired registration', async () => {
        const sources = [
            { deviceId: 'read-only', name: 'Read only', permissions: ['read'] as const },
            { deviceId: 'no-read', name: 'No read', permissions: [] as const },
            { deviceId: 'expired', name: 'Expired', permissions: ['read', 'bidirectional'] as const },
        ]
        await render(snapshot({ sources, expiredSourceIds: ['expired'] }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'read-only'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()
        expect(['Copy everything', 'Get changes only', 'Two-way sync'].map((name) => button(name)?.disabled))
            .toEqual([false, false, true])

        select.value = 'no-read'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()
        expect(['Copy everything', 'Get changes only', 'Two-way sync'].map((name) => button(name)?.disabled))
            .toEqual([true, true, true])

        select.value = 'expired'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()
        expect(['Copy everything', 'Get changes only', 'Two-way sync'].map((name) => button(name)?.disabled))
            .toEqual([true, true, true])
        expect(target.textContent).toContain('Registration expired. Register again with a new link.')
    })

    it.each([
        ['port-unavailable', 'That port is already in use. Choose another port.'],
        ['invalid-configuration', 'Check the sharing method, port, and public address.'],
        ['cleanup-failed', 'Sharing stopped, but cleanup did not finish. Try stopping again.'],
        ['transport-changed', 'Registration expired. Register again with a new link.'],
    ] as const)('maps the safe sharing error %s to concrete localized copy', async (code, message) => {
        await render(snapshot({ source: { phase: 'error', latestError: code } }))
        expect(target.querySelector('[data-sync-card="sharing"]')?.textContent).toContain(message)
        expect(target.querySelector('[data-sync-card="sharing"]')?.textContent).not.toContain(code)
    })

    it('shows QR, localized countdown, and rotates using permissions selected before the link', async () => {
        vi.useFakeTimers(); vi.setSystemTime(new Date('2026-09-02T00:00:00Z'))
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F127.0.0.1%3A32145&session=00000000-0000-4000-8000-000000000001&manifest=' + 'a'.repeat(64) + '#claim=' + 'b'.repeat(64)
        await render(snapshot({ source: { phase: 'running', pairingUri: uri, expiresAtMs: Date.now() + 125_000 } }))
        await vi.advanceTimersByTimeAsync(0); await tick()
        expect(target.querySelector('img[alt="Register a new device"]')).not.toBeNull()
        expect(target.textContent).toContain('2 minutes 5 seconds left')
        target.querySelectorAll<HTMLInputElement>('[data-permissions] input')[1].click(); button('Create new link')!.click()
        await vi.waitFor(() => expect(controllerState.controller.rotateLink).toHaveBeenCalledWith({ read: true, bidirectional: true }))
    })

    it('removes the old QR immediately while a replacement QR is still generating', async () => {
        const uri = (claim: string) => 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F127.0.0.1%3A32145&session=00000000-0000-4000-8000-000000000001&manifest=' + 'a'.repeat(64) + '#claim=' + claim.repeat(64)
        await render(snapshot({ source: { phase: 'running', pairingUri: uri('b'), expiresAtMs: Date.now() + 60_000 } }))
        await vi.waitFor(() => expect(target.querySelector('img')).not.toBeNull())
        let release!: (value: string) => void
        qrCode.toDataURL.mockImplementationOnce(() => new Promise((resolve) => { release = resolve }))
        controllerState.emit(snapshot({ source: { phase: 'running', pairingUri: uri('c'), expiresAtMs: Date.now() + 60_000 } }))
        await tick()
        expect(target.querySelector('img')).toBeNull()
        await vi.waitFor(() => expect(release).toBeTypeOf('function'))
        release(`data:image/mock,${uri('c')}`)
        await vi.waitFor(() => expect(target.querySelector('img')).not.toBeNull())
    })

    it('preserves the rendered QR when polling repeats the same pairing URI', async () => {
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F127.0.0.1%3A32145&session=00000000-0000-4000-8000-000000000001&manifest=' + 'a'.repeat(64) + '#claim=' + 'b'.repeat(64)
        const running = snapshot({ source: { phase: 'running', pairingUri: uri, expiresAtMs: Date.now() + 60_000 } })
        await render(running)
        await vi.waitFor(() => expect(target.querySelector('img')).not.toBeNull())
        const calls = qrCode.toDataURL.mock.calls.length
        controllerState.emit(running); await tick()
        expect(target.querySelector('img')).not.toBeNull()
        expect(qrCode.toDataURL).toHaveBeenCalledTimes(calls)
    })

    it('blocks every receive action while sharing or any recovered work is active', async () => {
        const cases: DeviceSyncControllerSnapshot[] = [
            snapshot({ source: { phase: 'preparing' } }),
            snapshot({ targets: { clone: { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'downloading' } } }, delta: deltaBase, bidirectional: bidiBase } }),
            snapshot({ targets: { clone: cloneBase, delta: { ...deltaBase, pullPhase: 'running' }, bidirectional: bidiBase } }),
            snapshot({ targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'targetPrepared', operationRetained: true } } }),
        ]
        for (const state of cases) {
            if (mounted) await unmount(mounted); mounted = undefined; target.replaceChildren(); await render(state)
            expect(['Copy everything', 'Get changes only', 'Two-way sync'].map((name) => button(name)?.disabled)).toEqual([true, true, true])
            if (state.source.phase === 'idle') expect(button('Start sharing')?.disabled).toBe(true)
        }
    })

    it('keeps the newly-started lane visible instead of stale snapshots from another lane', async () => {
        let release!: () => void
        controllerState.controller.pullRegisteredDelta.mockImplementationOnce(() => new Promise((resolve) => { release = () => resolve(undefined) }))
        await render(snapshot({ sources: [{ deviceId: 'source-a', name: 'Source A', permissions: ['read'], totalBytes: 2048 }], targets: { clone: { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'completed' } } }, delta: deltaBase, bidirectional: bidiBase } }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!; select.value = 'source-a'; select.dispatchEvent(new Event('change', { bubbles: true }))
        button('Get changes only')!.click(); await tick()
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('Receiving')
        expect(target.querySelector('[data-work-status]')?.textContent).not.toContain('previous data is kept')
        release()
    })

    it('requires clone confirmation and runs claim, confirm, then download in order', async () => {
        alerts.alertConfirm.mockResolvedValueOnce(false)
        await render()
        button('Copy everything')!.click(); await tick()
        expect(controllerState.controller.claimStagedClone).not.toHaveBeenCalled()
        alerts.alertConfirm.mockResolvedValueOnce(true)
        const order: string[] = []
        controllerState.controller.claimStagedClone.mockImplementationOnce(async () => { order.push('claim') })
        controllerState.controller.confirmCloneReplace.mockImplementationOnce(async () => { order.push('confirm') })
        controllerState.controller.downloadClone.mockImplementationOnce(async () => { order.push('download') })
        const link = target.querySelector<HTMLInputElement>('#device-sync-link')!
        link.value = 'risuailocal://peer-clone/v2?test'; link.dispatchEvent(new Event('input', { bubbles: true }))
        button('Copy everything')!.click()
        await vi.waitFor(() => expect(order).toEqual(['claim', 'confirm', 'download']))
        expect(controllerState.controller.stageLink).toHaveBeenCalledWith('risuailocal://peer-clone/v2?test')
    })

    it('selects a registered clone before confirmation and download', async () => {
        const order: string[] = []
        controllerState.controller.selectRegisteredClone.mockImplementationOnce(async () => { order.push('select') })
        controllerState.controller.confirmCloneReplace.mockImplementationOnce(async () => { order.push('confirm') })
        controllerState.controller.downloadClone.mockImplementationOnce(async () => { order.push('download') })
        await render(snapshot({ sources: [{ deviceId: 'source-a', name: 'Source A', permissions: ['read'] }] }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'source-a'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()
        button('Copy everything')!.click()
        await vi.waitFor(() => expect(order).toEqual(['select', 'confirm', 'download']))
        expect(controllerState.controller.selectRegisteredClone).toHaveBeenCalledWith('source-a')
    })

    it('wires clone cancellation to the controller once', async () => {
        const clone = { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'downloading' as const, completedBytes: 50, totalBytes: 100 } } }
        await render(snapshot({ targets: { clone, delta: deltaBase, bidirectional: bidiBase } }))
        button('Cancel')!.click()
        await vi.waitFor(() => expect(controllerState.controller.cancelClone).toHaveBeenCalledOnce())
    })

    it('infers recovered active work ahead of stale terminal snapshots', async () => {
        await render(snapshot({ targets: {
            clone: { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'completed' } } },
            delta: { ...deltaBase, pullPhase: 'running' },
            bidirectional: { ...bidiBase, operationPhase: 'stale' },
        } }))
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('Receiving')
        expect(target.querySelector('[data-work-status]')?.textContent).not.toContain('changed after sync started')
    })

    it('maps terminal results with human-readable totals and backup receipts', async () => {
        await render(snapshot({ targets: { clone: cloneBase, delta: { ...deltaBase, pullPhase: 'completed', pullResult: { kind: 'updated', revision: 2, transferredObjects: 3, transferredBytes: 2048 } }, bidirectional: bidiBase } }))
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('Received 3 items (2.0 KiB).')
        await unmount(mounted!); mounted = undefined; target.replaceChildren()
        await render(snapshot({ targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'completed', operationResult: { kind: 'updated', operationId: 'op', revision: 2, remoteRevision: 3, transferredObjects: 4, transferredBytes: 4096, backups: [{ packageId: 'p', side: 'local', path: 'safe/backup.risulossless' }] } } } }))
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('Received 4 items (4.0 KiB).')
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('Your previous data is kept as a backup.')
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('safe/backup.risulossless')
    })

    it('keeps a locally dismissed terminal result hidden across unrelated snapshots', async () => {
        const completed = snapshot({ targets: { clone: cloneBase, delta: { ...deltaBase, pullPhase: 'completed', pullResult: { kind: 'noChanges', revision: 1, transferredObjects: 0, transferredBytes: 0 } }, bidirectional: bidiBase } })
        await render(completed)
        button('Dismiss')!.click(); await tick()
        expect(target.querySelector('[data-work-status]')).toBeNull()
        controllerState.emit({ ...completed, source: { phase: 'error', latestError: 'state-unavailable' } })
        await tick()
        expect(target.querySelector('[data-work-status]')).toBeNull()
    })

    it('shows clone backup paths only when the completed snapshot has receipts', async () => {
        const completed = { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'completed' as const, completedBytes: 1024 } } }
        await render(snapshot({ targets: { clone: completed, delta: deltaBase, bidirectional: bidiBase } }))
        expect(target.querySelector('[data-work-status]')?.textContent).not.toContain('previous data is kept')
        await unmount(mounted!); mounted = undefined; target.replaceChildren()
        const withReceipt = { ...completed, state: { ...completed.state, target: { ...completed.state.target, backupPaths: ['safe/clone-backup.risulossless'] } } }
        await render(snapshot({ targets: { clone: withReceipt, delta: deltaBase, bidirectional: bidiBase } }))
        const status = target.querySelector('[data-work-status]')?.textContent ?? ''
        expect(status).toContain('previous data is kept')
        expect(status).toContain('safe/clone-backup.risulossless')
    })

    it.each([
        ['downloading', false, 'Receiving 50%', 'Cancel'],
        ['cancelled', false, 'Continue', 'Continue'],
        ['joined', true, 'Continue', 'Continue'],
        ['failed', false, 'Error', ''],
    ] as const)('maps clone %s with resumeAvailable=%s', async (phase, resumeAvailable, expected, action) => {
        const clone = { ...cloneBase, resumeAvailable, platform: phase === 'joined' ? 'android' as const : 'desktop' as const, error: phase === 'failed' ? 'operation-failed' as const : null, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase, completedBytes: 50, totalBytes: 100 } } }
        await render(snapshot({ targets: { clone, delta: deltaBase, bidirectional: bidiBase } }))
        expect(target.querySelector('[data-work-status]')?.textContent).toContain(expected)
        if (action) expect(button(action)).not.toBeUndefined()
        if (phase === 'failed') expect(button('Continue')).toBeUndefined()
    })

    it('uses indeterminate clone progress when the total size is unknown', async () => {
        const clone = { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'downloading' as const, completedBytes: 50 } } }
        await render(snapshot({ targets: { clone, delta: deltaBase, bidirectional: bidiBase } }))
        const status = target.querySelector('[data-work-status]')?.textContent ?? ''
        expect(status).toContain('Progress')
        expect(status).not.toContain('Receiving 100%')
        expect(target.querySelector('progress')?.hasAttribute('value')).toBe(false)
    })

    it.each([
        ['running', undefined, 'Receiving 0%'],
        ['completed', { kind: 'noChanges', revision: 1, transferredObjects: 0, transferredBytes: 0 }, 'Already up to date.'],
        ['fullCloneRequired', { kind: 'fullCloneRequired', reason: 'noExactCommonBase' }, 'A full copy is required.'],
        ['conflict', { kind: 'conflict', reason: 'localAndRemoteChanged' }, 'Both devices changed the same logical records.'],
        ['failed', undefined, 'Error'],
    ] as const)('maps delta %s safely', async (phase, pullResult, expected) => {
        const delta = { ...deltaBase, pullPhase: phase, pullResult, error: phase === 'failed' ? 'operation-failed' as const : null }
        await render(snapshot({ targets: { clone: cloneBase, delta, bidirectional: bidiBase } }))
        expect(target.querySelector('[data-work-status]')?.textContent).toContain(expected)
    })

    it('resolves known conflict names and aggregates opaque keys without leaking IDs', async () => {
        const characterKey = encodeLogicalRecordKey({ kind: 'character', characterId: 'char-a' })
        const conversationKey = encodeLogicalRecordKey({ kind: 'conversation', characterId: 'char-a', conversationId: 'chat-a' })
        const opaque = ['r1:root', 'not-a-key', encodeLogicalRecordKey({ kind: 'asset', logicalKey: 'secret-file-id' })]
        await render(snapshot({ targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'awaitingConflict', operationRetained: true, operationResult: { kind: 'conflict', operationId: 'op', conflicts: [characterKey, conversationKey, ...opaque].map((key) => ({ key, type: 'sameRecord' as const })), localManifestHash: 'local-secret', remoteManifestHash: 'remote-secret' } } } }))
        const status = target.querySelector('[data-work-status]')!.textContent ?? ''
        expect(status).toContain('Aster'); expect(status).toContain('First meeting'); expect(status).toContain('3 other items')
        expect(status).not.toContain('char-a'); expect(status).not.toContain('secret-file-id'); expect(status).not.toContain('local-secret')
    })

    it.each([
        ['sourceUnavailable', true, true], ['refreshPending', true, false], ['localCommitted', true, true], ['targetPrepared', true, true], ['sourcePrepared', true, true], ['failed', false, false], ['stale', false, false],
    ] as const)('renders guarded bidirectional actions for %s', async (phase, resume, abandon) => {
        await render(snapshot({ targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: phase, operationRetained: !['failed', 'stale'].includes(phase), operationError: phase === 'failed' ? 'operation-failed' : null } } }))
        expect(Boolean(button('Continue'))).toBe(resume); expect(Boolean(button('Abandon task'))).toBe(abandon)
        if (phase === 'sourcePrepared') expect(target.querySelector('[data-work-status]')?.textContent).toContain('Start sharing')
        if (phase === 'stale') expect(target.querySelector('[data-work-status]')?.textContent).toContain('changed after sync started')
    })

    it.each([
        ['idle', ['prepare', 'start', 'resume']],
        ['prepared', ['start', 'resume']],
        ['running', ['resume']],
    ] as const)('continues sourcePrepared from a %s sharing phase', async (sourcePhase, expected) => {
        const calls: string[] = []
        controllerState.controller.prepare.mockImplementationOnce(async () => { calls.push('prepare') })
        controllerState.controller.start.mockImplementationOnce(async () => { calls.push('start') })
        controllerState.controller.resumeBidirectional.mockImplementationOnce(async () => { calls.push('resume') })
        await render(snapshot({ source: { phase: sourcePhase }, targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'sourcePrepared', operationRetained: true } } }))
        button('Continue')!.click()
        await vi.waitFor(() => expect(calls).toEqual(expected))
        if (sourcePhase === 'idle') expect(controllerState.controller.prepare).toHaveBeenCalledWith({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })
    })

    it('gates conflict resolution while the first winner is pending', async () => {
        let release!: () => void
        controllerState.controller.resolveRegisteredBidirectional.mockImplementationOnce(() => new Promise((resolve) => { release = () => resolve(undefined) }))
        await render(snapshot({ activeBidirectionalSourceDeviceId: 'source-a', targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'awaitingConflict', operationRetained: true, operationResult: { kind: 'conflict', operationId: 'op', conflicts: [], localManifestHash: 'local', remoteManifestHash: 'remote' } } } }))
        button('Keep this device')!.click(); await tick()
        expect(button('Keep other device')).toBeUndefined()
        expect(controllerState.controller.resolveRegisteredBidirectional).toHaveBeenCalledTimes(1)
        release()
    })

    it('confirms abandonment and acknowledges a completed bidirectional result', async () => {
        await render(snapshot({ targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'targetPrepared', operationRetained: true } } }))
        button('Abandon task')!.click()
        await vi.waitFor(() => expect(controllerState.controller.abandonBidirectional).toHaveBeenCalledOnce())
        expect(alerts.alertConfirm).toHaveBeenCalledWith('Cancel this sync? Nothing received so far will be applied.')
        await unmount(mounted!); mounted = undefined; target.replaceChildren()
        await render(snapshot({ targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'completed', operationRetained: true, operationResult: { kind: 'noChanges', operationId: 'op', revision: 1, remoteRevision: 1, transferredObjects: 0, transferredBytes: 0, backups: [] } } } }))
        button('Dismiss')!.click()
        await vi.waitFor(() => expect(target.querySelector('[data-work-status]')).toBeNull())
        expect(controllerState.controller.acknowledgeBidirectional).toHaveBeenCalledOnce()
    })
})
