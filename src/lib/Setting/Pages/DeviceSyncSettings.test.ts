// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import type { DeviceSyncControllerSnapshot } from 'src/ts/storage/sync/deviceSyncController'
import { encodeLogicalRecordKey } from 'src/ts/storage/sync/logicalRecordKey'
import deviceSyncRawSource from './DeviceSyncSettings.svelte?raw'

const deviceSyncSource = deviceSyncRawSource.replace(/\r\n?/g, '\n')

const environment = vi.hoisted(() => ({ android: false, notifications: true }))
const settingsState = vi.hoisted(() => ({
    value: { syncListenMethod: 'lan', syncFixedPort: 32145, syncPublicBaseUrl: '', syncAutoListen: false },
}))
const alerts = vi.hoisted(() => ({ alertConfirm: vi.fn(async () => true) }))
const database = vi.hoisted(() => ({
    characters: [{ chaId: 'char-a', name: 'Aster', chats: [{ id: 'chat-a', name: 'First meeting' }] }],
}))
const qrCode = vi.hoisted(() => ({ toDataURL: vi.fn<(uri: string) => Promise<string>>() }))
const controllerState = vi.hoisted(() => {
    let listener: ((snapshot: unknown) => void) | undefined
    const controller = {
        snapshot: vi.fn(), subscribe: vi.fn((next: (snapshot: unknown) => void) => { listener = next; return () => { listener = undefined } }),
        initialize: vi.fn(async () => undefined), prepare: vi.fn(async () => undefined), start: vi.fn(async () => undefined), stop: vi.fn(async () => undefined), rotateLink: vi.fn(async () => undefined),
        revokeIncoming: vi.fn(async () => undefined), revokeOutgoing: vi.fn(async () => undefined), stageLink: vi.fn(), clearStagedLink: vi.fn(), claimStagedClone: vi.fn(async () => undefined), selectRegisteredClone: vi.fn(async () => undefined),
        confirmCloneReplace: vi.fn(async () => undefined), downloadClone: vi.fn(async () => undefined), resumeClone: vi.fn(async () => undefined), cancelClone: vi.fn(async () => undefined),
        pullStagedDelta: vi.fn(async () => undefined), pullRegisteredDelta: vi.fn(async () => undefined), syncStagedBidirectional: vi.fn(async () => undefined), syncRegisteredBidirectional: vi.fn(async () => undefined),
        resolveRegisteredBidirectional: vi.fn(async () => undefined), resumeBidirectional: vi.fn(async () => undefined), rehostBidirectionalSource: vi.fn(async () => undefined), acknowledgeBidirectional: vi.fn(async () => undefined), abandonBidirectional: vi.fn(async () => undefined), abandonDelta: vi.fn(async () => undefined),
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
    targetPhase: 'idle' as const, error: null, warning: null,
    state: { target: { phase: 'idle' as const, destructiveConfirmed: false, completedBytes: 0 } },
}
const deltaBase = { pullPhase: 'idle' as const, retained: null, error: null }
const bidiBase = { operationPhase: 'idle' as const, operationRetained: false, operationError: null }
const genericFailure = 'The task did not finish. Try again, and if it keeps failing restart the app on both devices.'
const validRegistrationUri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F10.1.2.3%3A32145&session=00000000-0000-4000-8000-000000000001&manifest=' + 'a'.repeat(64) + '#claim=' + 'b'.repeat(64)

function snapshot(partial: Partial<DeviceSyncControllerSnapshot> = {}): DeviceSyncControllerSnapshot {
    return { source: { phase: 'idle' }, sources: [], devices: [], error: null, sourceError: null, workError: null, remoteCommitNotice: null, stagedLink: null, stagedUri: null, stagedSourceDeviceId: null, activeCloneSourceDeviceId: null, activeBidirectionalSourceDeviceId: null, expiredSourceIds: [], targets: { clone: cloneBase, delta: deltaBase, bidirectional: bidiBase }, ...partial }
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

    it('uses the redesigned controller page without legacy embeds or receipt casts', () => {
        expect(deviceSyncSource).not.toContain('cloneTarget as unknown')
        expect(deviceSyncSource).toContain('cloneTarget?.backupPaths')
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
        await vi.waitFor(() => expect(target.querySelector('[data-share-error]')?.textContent).toBe(genericFailure))
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

    it('reports a pending remote commit refresh inside the sharing card', async () => {
        await render(snapshot({ source: { phase: 'running' }, remoteCommitNotice: 'refreshFailed' }))

        const notice = target.querySelector('[data-share-refresh]')!
        expect(notice.textContent).toBe('Both devices are committed, but the local screen could not refresh.')
        expect(target.querySelector('[data-sync-card="sharing"]')?.contains(notice)).toBe(true)
    })

    it('reports discarded edits inside the sharing card', async () => {
        await render(snapshot({ source: { phase: 'running' }, remoteCommitNotice: 'editsDiscarded' }))

        const notice = target.querySelector('[data-share-refresh]')!
        expect(notice.textContent).toBe("The other device's changes were applied, and recent unsaved edits on this device were lost. The screen may differ from what you last saw.")
        expect(target.querySelector('[data-sync-card="sharing"]')?.contains(notice)).toBe(true)
    })

    it('lets a source error win over the pending remote commit refresh notice', async () => {
        await render(snapshot({
            source: { phase: 'running' }, sourceError: 'port-unavailable', remoteCommitNotice: 'refreshFailed',
        }))

        expect(target.querySelector('[data-share-refresh]')).toBeNull()
        expect(target.querySelector('[data-sync-card="sharing"]')?.querySelector('[role="alert"]')?.textContent)
            .toBe('That port is already in use. Choose another port.')
    })

    it('lets a sharing action error win over the pending remote commit refresh notice', async () => {
        controllerState.controller.stop.mockRejectedValueOnce('port-unavailable')
        await render(snapshot({ source: { phase: 'running' }, remoteCommitNotice: 'refreshFailed' }))

        button('Stop sharing')!.click()

        await vi.waitFor(() => expect(target.querySelector('[data-share-error]')?.textContent)
            .toBe('That port is already in use. Choose another port.'))
        expect(target.querySelector('[data-share-refresh]')).toBeNull()
    })

    it('keeps a receive action error inside the work card', async () => {
        controllerState.controller.pullStagedDelta.mockImplementationOnce(async () => {
            controllerState.emit(snapshot({ error: 'operation-failed' }))
            throw new Error('private target detail')
        })
        await render(snapshot({ stagedSourceDeviceId: 'source-a' }))

        button('Get changes only')!.click()

        await vi.waitFor(() => expect(target.querySelector('[data-work-error]')?.textContent).toBe(genericFailure))
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
        expect(removeButtons.map((candidate) => candidate.getAttribute('aria-label'))).toEqual([
            'Remove: Outgoing, Devices that copy from this device',
            'Remove: Incoming, Devices this device copies from',
        ])
        removeButtons[0].click(); await vi.waitFor(() => expect(controllerState.controller.revokeOutgoing).toHaveBeenCalledWith('out-a'))
        removeButtons[1].click(); await vi.waitFor(() => expect(controllerState.controller.revokeIncoming).toHaveBeenCalledWith('in-a'))
        expect(target.textContent).toContain('2.0 KiB total')
        expect(target.querySelectorAll('[data-device-icon]')).toHaveLength(2)
    })

    it('shows a pending registration link that the controller consumed before page mount', async () => {
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F127.0.0.1%3A32145&session=00000000-0000-4000-8000-000000000001&manifest=' + 'a'.repeat(64) + '#claim=' + 'b'.repeat(64)
        await render(snapshot({ stagedUri: uri, stagedLink: { endpoint: 'http://127.0.0.1:32145', sessionId: 'session', manifestId: 'a'.repeat(64), claim: 'b'.repeat(64) } }))

        expect(target.querySelector<HTMLSelectElement>('#device-sync-target')?.value).toBe('new-link')
        expect(target.querySelector<HTMLInputElement>('#device-sync-link')?.value).toBe(uri)
        expect(controllerState.controller.stageLink).not.toHaveBeenCalled()
    })

    it('updates the mounted input from controller-owned links and stages only a genuine manual replacement', async () => {
        const pending = 'risuailocal://peer-clone/v2?pending'
        const replacement = validRegistrationUri
        await render(snapshot())

        controllerState.emit(snapshot({ stagedUri: pending }))
        await tick()
        expect(target.querySelector<HTMLInputElement>('#device-sync-link')?.value).toBe(pending)

        const link = target.querySelector<HTMLInputElement>('#device-sync-link')!
        link.value = replacement
        link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        button('Get changes only')!.click()

        await vi.waitFor(() => expect(controllerState.controller.pullStagedDelta).toHaveBeenCalledOnce())
        expect(controllerState.controller.stageLink).toHaveBeenCalledExactlyOnceWith(replacement)
    })

    it('clears controller-owned staging and blocks receive when the new-link input is cleared', async () => {
        await render(snapshot({
            stagedUri: validRegistrationUri,
            stagedLink: { endpoint: 'http://10.1.2.3:32145/', sessionId: '00000000-0000-4000-8000-000000000001', manifestId: 'a'.repeat(64), claim: 'b'.repeat(64) },
        }))
        const link = target.querySelector<HTMLInputElement>('#device-sync-link')!

        link.value = ''
        link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()

        expect(controllerState.controller.clearStagedLink).toHaveBeenCalledOnce()
        expect(['Copy everything', 'Get changes only', 'Two-way sync'].map((name) => button(name)?.disabled))
            .toEqual([true, true, true])
    })

    it('allows new-link work only for a valid link or an explicitly retained retry', async () => {
        await render()
        expect(button('Get changes only')?.disabled).toBe(true)

        const link = target.querySelector<HTMLInputElement>('#device-sync-link')!
        link.value = 'not-a-registration-link'
        link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        expect(button('Get changes only')?.disabled).toBe(true)

        link.value = validRegistrationUri
        link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        expect(button('Get changes only')?.disabled).toBe(false)

        await unmount(mounted!); mounted = undefined; target.replaceChildren()
        await render(snapshot({ stagedSourceDeviceId: 'source-a' }))
        expect(button('Get changes only')?.disabled).toBe(false)
    })

    it('clears stale staging after a completed receive action', async () => {
        await render()
        const link = target.querySelector<HTMLInputElement>('#device-sync-link')!
        link.value = validRegistrationUri
        link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()

        button('Get changes only')!.click()

        await vi.waitFor(() => expect(controllerState.controller.pullStagedDelta).toHaveBeenCalledOnce())
        await vi.waitFor(() => expect(controllerState.controller.clearStagedLink).toHaveBeenCalledOnce())
        expect(link.value).toBe('')
    })

    it('retries a failed post-claim receive without replacing the retained source', async () => {
        await render(snapshot({ stagedSourceDeviceId: 'source-a' }))

        button('Get changes only')!.click()

        await vi.waitFor(() => expect(controllerState.controller.pullStagedDelta).toHaveBeenCalledOnce())
        expect(controllerState.controller.stageLink).not.toHaveBeenCalled()
    })

    it('resets a selected incoming source after it is removed', async () => {
        await render(snapshot({ sources: [{ deviceId: 'source-a', name: 'Source A', permissions: ['read'] }] }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'source-a'
        select.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()

        controllerState.emit(snapshot({ sources: [] }))
        await tick()

        expect(select.value).toBe('new-link')
        expect(target.querySelector('#device-sync-link')).not.toBeNull()
    })

    it('resets a selected incoming source immediately after revocation', async () => {
        await render(snapshot({ sources: [{ deviceId: 'source-a', name: 'Source A', permissions: ['read'] }] }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'source-a'
        select.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()

        const remove = [...target.querySelectorAll<HTMLButtonElement>('button')]
            .find((candidate) => candidate.getAttribute('aria-label')?.startsWith('Remove: Source A'))!
        remove.click()

        await vi.waitFor(() => expect(controllerState.controller.revokeIncoming).toHaveBeenCalledWith('source-a'))
        expect(select.value).toBe('new-link')
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
        await tick()
        button('Get changes only')!.click(); await tick()
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('Receiving')
        expect(target.querySelector('[data-work-status]')?.textContent).not.toContain('previous data is kept')
        release()
    })

    it('requires clone confirmation and runs claim, confirm, then download in order', async () => {
        await render()
        const link = target.querySelector<HTMLInputElement>('#device-sync-link')!
        link.value = validRegistrationUri; link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        alerts.alertConfirm.mockResolvedValueOnce(false)
        button('Copy everything')!.click(); await tick()
        expect(controllerState.controller.claimStagedClone).not.toHaveBeenCalled()
        alerts.alertConfirm.mockResolvedValueOnce(true)
        const order: string[] = []
        controllerState.controller.claimStagedClone.mockImplementationOnce(async () => { order.push('claim') })
        controllerState.controller.confirmCloneReplace.mockImplementationOnce(async () => { order.push('confirm') })
        controllerState.controller.downloadClone.mockImplementationOnce(async () => { order.push('download') })
        button('Copy everything')!.click()
        await vi.waitFor(() => expect(order).toEqual(['claim', 'confirm', 'download']))
        expect(controllerState.controller.stageLink).toHaveBeenCalledWith(validRegistrationUri)
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
            bidirectional: { ...bidiBase, operationPhase: 'completed' },
        } }))
        expect(target.querySelector('[data-work-status]')?.textContent).toContain('Receiving')
        expect(target.querySelector('[data-work-status]')?.textContent).not.toContain('Already up to date')
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
        ['failed', false, 'did not finish', 'Continue'],
    ] as const)('maps clone %s with resumeAvailable=%s', async (phase, resumeAvailable, expected, action) => {
        const clone = { ...cloneBase, resumeAvailable, platform: phase === 'joined' ? 'android' as const : 'desktop' as const, error: phase === 'failed' ? 'operation-failed' as const : null, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase, completedBytes: 50, totalBytes: 100 } } }
        await render(snapshot({ targets: { clone, delta: deltaBase, bidirectional: bidiBase } }))
        expect(target.querySelector('[data-work-status]')?.textContent).toContain(expected)
        if (action) expect(button(action)).not.toBeUndefined()
    })

    it.each(['failed', 'cancelled'] as const)('releases or dismisses an Android %s clone', async (phase) => {
        environment.android = true
        const clone = { ...cloneBase, platform: 'android' as const, error: phase === 'failed' ? 'operation-failed' as const : null, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase } } }
        await render(snapshot({ targets: { clone, delta: deltaBase, bidirectional: bidiBase } }))

        button('Dismiss')!.click()
        await vi.waitFor(() => expect(target.querySelector('[data-work-status]')).toBeNull())

        expect(controllerState.controller.cancelClone).toHaveBeenCalledTimes(phase === 'failed' ? 1 : 0)
    })

    it('uses indeterminate clone progress when the total size is unknown', async () => {
        const clone = { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'downloading' as const, completedBytes: 50 } } }
        await render(snapshot({ targets: { clone, delta: deltaBase, bidirectional: bidiBase } }))
        const status = target.querySelector('[data-work-status]')?.textContent ?? ''
        expect(status).toContain('Progress')
        expect(status).not.toContain('Receiving 100%')
        expect(target.querySelector('progress')?.hasAttribute('value')).toBe(false)
        expect(target.querySelector('progress')?.getAttribute('aria-label')).toBe('Progress')
    })

    it.each([
        ['running', undefined, 'Receiving...'],
        ['completed', { kind: 'noChanges', revision: 1, transferredObjects: 0, transferredBytes: 0 }, 'Already up to date.'],
        ['fullCloneRequired', { kind: 'fullCloneRequired', reason: 'noExactCommonBase' }, 'A full copy is required.'],
        ['conflict', { kind: 'conflict', reason: 'localAndRemoteChanged' }, 'The same items changed on both devices, so the changes could not be received.'],
        ['failed', undefined, 'did not finish'],
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
        ['sourceUnavailable', true, true], ['refreshPending', true, false], ['localCommitted', true, true], ['targetPrepared', true, true], ['sourcePrepared', true, true], ['failed', false, false],
    ] as const)('renders guarded bidirectional actions for %s', async (phase, resume, abandon) => {
        await render(snapshot({ targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: phase, operationRetained: phase !== 'failed', operationError: phase === 'failed' ? 'operation-failed' : null } } }))
        expect(Boolean(button('Continue'))).toBe(resume); expect(Boolean(button('Abandon task'))).toBe(abandon)
        if (phase === 'sourcePrepared') expect(target.querySelector('[data-work-status]')?.textContent).toContain('Start sharing')
    })

    it.each(['idle', 'prepared'] as const)('delegates sourcePrepared rehosting from a %s sharing phase', async (sourcePhase) => {
        await render(snapshot({ source: { phase: sourcePhase }, targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'sourcePrepared', operationRetained: true } } }))
        button('Continue')!.click()
        await vi.waitFor(() => expect(controllerState.controller.rehostBidirectionalSource).toHaveBeenCalledWith(
            { method: 'lan', fixedPort: 32145, publicBaseUrl: '' },
            { read: true, bidirectional: false },
        ))
        expect(controllerState.controller.prepare).not.toHaveBeenCalled()
        expect(controllerState.controller.start).not.toHaveBeenCalled()
        expect(controllerState.controller.resumeBidirectional).not.toHaveBeenCalled()
    })

    it('disables sourcePrepared Continue while the rehosted source is already running', async () => {
        await render(snapshot({ source: { phase: 'running' }, targets: { clone: cloneBase, delta: deltaBase, bidirectional: { ...bidiBase, operationPhase: 'sourcePrepared', operationRetained: true } } }))
        expect(button('Continue')?.disabled).toBe(true)
    })

    it('does not move a dismissed receive error into the sharing card', async () => {
        await render(snapshot({
            error: 'operation-failed', workError: 'operation-failed',
            targets: { clone: cloneBase, delta: { ...deltaBase, pullPhase: 'failed', error: 'operation-failed' }, bidirectional: bidiBase },
        }))

        button('Dismiss')!.click()
        await tick()

        expect(target.querySelector('[data-sync-card="sharing"]')?.querySelector('[role="alert"]')).toBeNull()
    })

    it('renders only the prioritized localized work error when lane and controller errors overlap', async () => {
        await render(snapshot({
            workError: 'registration-expired',
            targets: { clone: cloneBase, delta: { ...deltaBase, pullPhase: 'failed', error: 'operation-failed' }, bidirectional: bidiBase },
        }))

        const alerts = target.querySelectorAll('[data-sync-card="work"] [role="alert"]')
        expect(alerts).toHaveLength(1)
        expect(alerts[0]?.textContent).toBe('Registration expired. Register again with a new link.')
        expect(target.querySelector('[data-work-status]')?.textContent).not.toContain('Error')
    })

    it('tells the user to finish the current operation when a registration is refused', async () => {
        await render(snapshot({
            workError: 'registration-blocked-by-active-work',
            targets: {
                clone: cloneBase,
                delta: deltaBase,
                bidirectional: { ...bidiBase, operationPhase: 'targetPrepared', operationRetained: true },
            },
        }))

        expect(target.querySelector('[data-sync-card="work"] [data-work-error]')?.textContent)
            .toBe('Finish or stop the current operation before registering a new device.')
        expect(target.querySelector('[data-sync-card="work"]')?.textContent)
            .not.toContain('registration-blocked-by-active-work')
    })

    it.each([
        ['source-in-use' as const, 'A sync task is still using this device. Finish or cancel it, then try again.'],
        ['source-changed' as const, "The target device's registration changed. Select the target device again and retry."],
        ['delta-completion-retained' as const, 'An unfinished earlier update is still pending. Continue or cancel it under Sync tasks.'],
        ['peer-outdated' as const, 'The other device runs an older RisuNest. Update both devices to the same version and try again.'],
    ])('renders the localized wording for the bounded work failure %s', async (code, wording) => {
        await render(snapshot({
            workError: code,
            targets: { clone: cloneBase, delta: { ...deltaBase, pullPhase: 'failed' }, bidirectional: bidiBase },
        }))

        expect(target.querySelector('[data-sync-card="work"] [data-work-error]')?.textContent).toBe(wording)
        expect(target.querySelector('[data-sync-card="work"]')?.textContent).not.toContain(code)
    })

    it('renders one work alert when the selected source and operation are both expired', async () => {
        await render(snapshot({
            sources: [{ deviceId: 'source-a', name: 'Source A', permissions: ['read'] }],
            expiredSourceIds: ['source-a'],
            workError: 'registration-expired',
            targets: { clone: cloneBase, delta: { ...deltaBase, pullPhase: 'failed', error: 'registration-expired' }, bidirectional: bidiBase },
        }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'source-a'
        select.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()

        const alerts = target.querySelectorAll('[data-sync-card="work"] [role="alert"]')
        expect(alerts).toHaveLength(1)
        expect(alerts[0]?.textContent).toBe('Registration expired. Register again with a new link.')
    })

    it('uses project theme tokens instead of raw red and yellow colors', async () => {
        await render()
        const warning = target.querySelector('[data-lan-warning]')!
        expect(warning.className).toContain('border-darkborderc')
        expect(warning.className).toContain('bg-selected')

        await unmount(mounted!); mounted = undefined; target.replaceChildren()
        await render(snapshot({ devices: [{ deviceId: 'out-a', name: 'Outgoing', permissions: ['read'] }] }))
        const remove = [...target.querySelectorAll<HTMLButtonElement>('button')]
            .find((candidate) => candidate.getAttribute('aria-label')?.startsWith('Remove: Outgoing'))!
        expect(`${warning.className} ${remove.className}`).not.toMatch(/(?:red|yellow)-\d/)
        expect(remove.className).toContain('bg-draculared')
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
    const retainedDelta = (overrides: Record<string, unknown> = {}) => ({
        operationId: '00000000-0000-4000-8000-000000000091',
        sourceDeviceId: 'source-a',
        sourceName: 'Desk',
        witness: 'ambiguous' as const,
        transferredObjects: 2,
        transferredBytes: 4096,
        ...overrides,
    })
    const registeredSourceA = [{ deviceId: 'source-a', name: 'Source A', permissions: ['read'] as const }]

    it.each([
        ['ambiguous' as const, 'cannot continue', 'Turn the other device back on'],
        ['uncommitted' as const, 'Turn the other device back on', 'cannot continue'],
        ['committed' as const, 'Turn the other device back on', 'cannot continue'],
    ])('explains a retained %s delta by the other device name', async (witness, shown, hidden) => {
        await render(snapshot({
            sources: registeredSourceA,
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta({ witness }) }, bidirectional: bidiBase },
        }))

        const retained = target.querySelector('[data-delta-retained]')!
        expect(retained.textContent).toContain('An earlier update from Desk did not finish.')
        expect(target.querySelector('[data-work-status]')!.textContent).toContain(shown)
        expect(target.querySelector('[data-work-status]')!.textContent).not.toContain(hidden)
    })

    it('falls back to the unknown device wording when the source is no longer registered', async () => {
        await render(snapshot({
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta({ sourceName: null }) }, bidirectional: bidiBase },
        }))

        expect(target.querySelector('[data-delta-retained]')!.textContent)
            .toContain('An earlier update from Unknown device did not finish.')
    })

    it.each([
        ['uncommitted' as const, registeredSourceA, true],
        ['ambiguous' as const, registeredSourceA, false],
        ['uncommitted' as const, [], false],
    ])('offers continuing a retained %s delta only for a still registered source', async (witness, sources, offered) => {
        await render(snapshot({
            sources,
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta({ witness }) }, bidirectional: bidiBase },
        }))

        expect(Boolean(button('Continue'))).toBe(offered)
        expect(button('Abandon task')).toBeDefined()
        if (!offered) return
        button('Continue')!.click()
        await vi.waitFor(() => expect(controllerState.controller.pullRegisteredDelta).toHaveBeenCalledWith('source-a'))
    })

    it.each([
        ['a source that may no longer be read', [{ deviceId: 'source-a', name: 'Source A', permissions: [] as const }], [] as string[]],
        ['an expired registration', registeredSourceA, ['source-a']],
    ])('withholds continuing a retained delta from %s', async (_case, sources, expiredSourceIds) => {
        await render(snapshot({
            sources,
            expiredSourceIds,
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta({ witness: 'uncommitted' }) }, bidirectional: bidiBase },
        }))

        expect(button('Continue')).toBeUndefined()
        expect(button('Abandon task')).toBeDefined()
    })

    it('shows the running pull instead of the retained panel', async () => {
        await render(snapshot({
            sources: registeredSourceA,
            targets: {
                clone: cloneBase,
                delta: { ...deltaBase, retained: retainedDelta(), pullPhase: 'running' as const },
                bidirectional: bidiBase,
            },
        }))

        expect(target.querySelector('[data-delta-retained]')).toBeNull()
        expect(button('Abandon task')).toBeUndefined()
        expect(target.querySelector('[data-work-status]')!.textContent).toContain('Receiving...')
    })

    it('locks both retained delta actions while this device shares, but never the stop control', async () => {
        const sharing = (phase: 'running' | 'idle') => snapshot({
            source: { phase },
            sources: registeredSourceA,
            targets: {
                clone: cloneBase,
                delta: { ...deltaBase, retained: retainedDelta({ witness: 'uncommitted' }) },
                bidirectional: bidiBase,
            },
        })
        await render(sharing('running'))

        expect(button('Continue')!.disabled).toBe(true)
        expect(button('Abandon task')!.disabled).toBe(true)
        expect(target.querySelector('[data-sync-card="work"]')!.textContent)
            .toContain('Stop sharing to run a receive task.')
        expect(button('Stop sharing')!.disabled).toBe(false)

        controllerState.emit(sharing('idle')); await tick()

        expect(button('Continue')!.disabled).toBe(false)
        expect(button('Abandon task')!.disabled).toBe(false)
    })

    it('confirms before abandoning a retained delta and then closes the panel', async () => {
        const cleared = snapshot({
            sources: registeredSourceA,
            targets: { clone: cloneBase, delta: deltaBase, bidirectional: bidiBase },
        })
        controllerState.controller.abandonDelta.mockImplementationOnce(async () => {
            controllerState.emit(cleared)
            return undefined
        })
        await render(snapshot({
            sources: registeredSourceA,
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta() }, bidirectional: bidiBase },
        }))

        button('Abandon task')!.click()

        await vi.waitFor(() => expect(controllerState.controller.abandonDelta).toHaveBeenCalledOnce())
        expect(alerts.alertConfirm).toHaveBeenCalledWith(
            "Cancel the pending update? Data on this device stays as it is, but the other device's transfer statistics may not update.",
        )
        await vi.waitFor(() => expect(target.querySelector('[data-work-status]')).toBeNull())
    })

    it('keeps the panel up when the target still reports the delta as retained', async () => {
        await render(snapshot({
            sources: registeredSourceA,
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta() }, bidirectional: bidiBase },
        }))

        button('Abandon task')!.click()

        await vi.waitFor(() => expect(controllerState.controller.abandonDelta).toHaveBeenCalledOnce())
        await tick()
        // The cancellation reported success without dropping the journal, so
        // the only way out has to stay on screen.
        expect(target.querySelector('[data-delta-retained]')).not.toBeNull()
    })

    it('keeps a declined retained delta abandonment untouched', async () => {
        alerts.alertConfirm.mockResolvedValueOnce(false)
        await render(snapshot({
            sources: registeredSourceA,
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta() }, bidirectional: bidiBase },
        }))

        button('Abandon task')!.click()
        await tick()

        expect(controllerState.controller.abandonDelta).not.toHaveBeenCalled()
        expect(target.querySelector('[data-delta-retained]')).not.toBeNull()
    })

    it('locks every receive task while a delta stays retained but never the device removal', async () => {
        await render(snapshot({
            sources: registeredSourceA,
            targets: { clone: cloneBase, delta: { ...deltaBase, retained: retainedDelta() }, bidirectional: bidiBase },
        }))

        for (const name of ['Copy everything', 'Get changes only', 'Two-way sync']) {
            expect(button(name)?.disabled).toBe(true)
        }
        const removal = [...target.querySelectorAll<HTMLButtonElement>('[data-sync-card="devices"] button')]
        expect(removal.length).toBeGreaterThan(0)
        expect(removal.every((candidate) => candidate.disabled)).toBe(false)
    })

    it('lets a terminal desktop clone be dismissed so the other tasks unlock again', async () => {
        const cancelled = { ...cloneBase, state: { ...cloneBase.state, target: { ...cloneBase.state.target, phase: 'cancelled' as const } } }
        await render(snapshot({ sources: registeredSourceA, targets: { clone: cancelled, delta: deltaBase, bidirectional: bidiBase } }))
        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'source-a'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()
        expect(button('Get changes only')?.disabled).toBe(true)
        expect(button('Start sharing')?.disabled).toBe(true)

        button('Dismiss')!.click()
        await vi.waitFor(() => expect(target.querySelector('[data-work-status]')).toBeNull())

        expect(controllerState.controller.cancelClone).not.toHaveBeenCalled()
        expect(button('Get changes only')?.disabled).toBe(false)
        expect(button('Start sharing')?.disabled).toBe(false)
    })

    it('explains a pasted link that is not a registration link', async () => {
        await render()
        const link = target.querySelector<HTMLInputElement>('#device-sync-link')!

        link.value = 'https://example.com/not-a-link'
        link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        expect(target.querySelector('[data-link-invalid]')?.textContent).toBe('That is not a registration link. Copy the whole link from the other device, or open its QR code.')

        link.value = validRegistrationUri
        link.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        expect(target.querySelector('[data-link-invalid]')).toBeNull()
    })

    it('names the missing target instead of leaving a recovered conflict choice inert', async () => {
        const conflict = { ...bidiBase, operationPhase: 'awaitingConflict' as const, operationRetained: true, operationResult: { kind: 'conflict' as const, operationId: 'op', conflicts: [], localManifestHash: 'local', remoteManifestHash: 'remote' } }
        await render(snapshot({
            sources: [{ deviceId: 'source-a', name: 'Source A', permissions: ['read', 'bidirectional'] }],
            targets: { clone: cloneBase, delta: deltaBase, bidirectional: conflict },
        }))

        expect(button('Keep this device')?.disabled).toBe(true)
        expect(target.querySelector('[data-conflict-target]')?.textContent)
            .toBe('Choose the other device under Target device first, then pick which side to keep.')

        const select = target.querySelector<HTMLSelectElement>('#device-sync-target')!
        select.value = 'source-a'; select.dispatchEvent(new Event('change', { bubbles: true })); await tick()

        expect(button('Keep this device')?.disabled).toBe(false)
        expect(target.querySelector('[data-conflict-target]')).toBeNull()
        button('Keep this device')!.click()
        await vi.waitFor(() => expect(controllerState.controller.resolveRegisteredBidirectional).toHaveBeenCalledWith('source-a', 'local'))
    })

    it('refuses to share a link that would grant nothing', async () => {
        await render()
        const permissionInputs = target.querySelectorAll<HTMLInputElement>('[data-permissions] input')

        permissionInputs[0].click(); await tick()

        expect(button('Start sharing')?.disabled).toBe(true)
        permissionInputs[1].click(); await tick()
        expect(button('Start sharing')?.disabled).toBe(false)
    })

    it('refuses a second link rotation instead of reporting an unavailable slot', async () => {
        let release!: () => void
        controllerState.controller.rotateLink.mockImplementationOnce(() => new Promise((resolve) => { release = () => resolve(undefined) }))
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F127.0.0.1%3A32145&session=00000000-0000-4000-8000-000000000001&manifest=' + 'a'.repeat(64) + '#claim=' + 'b'.repeat(64)
        await render(snapshot({ source: { phase: 'running', pairingUri: uri, expiresAtMs: Date.now() + 60_000 } }))

        button('Create new link')!.click(); await tick()

        expect(button('Create new link')?.disabled).toBe(true)
        button('Create new link')!.click(); await tick()
        expect(controllerState.controller.rotateLink).toHaveBeenCalledTimes(1)
        expect(target.querySelector('[data-share-error]')).toBeNull()
        release()
    })

    it('re-reads the Android notification warning when the page regains focus', async () => {
        environment.android = true
        await render()
        expect(target.querySelector('[data-notification-warning]')).toBeNull()

        environment.notifications = false
        window.dispatchEvent(new Event('focus'))
        await tick()

        expect(target.querySelector('[data-notification-warning]')).not.toBeNull()
        expect(button('Open system settings')).toBeDefined()
    })
})
