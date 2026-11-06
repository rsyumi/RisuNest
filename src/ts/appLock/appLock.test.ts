import { describe, expect, it, vi } from 'vitest'
import { appLock } from './appLock.svelte'
import { APP_LOCK_KEY, createPinRecord, defaultAppLockRecord } from './record'
import { createNativeDeviceMarkers, DEVICE_MARKER_KEYS, installDeviceMarkers } from '../storage/deviceMarkers'
import type { NativeDeviceSettings } from '../storage/nativeDeviceSettings'
import type { AppLockLifecycle, AppLockNativePolicy } from './native'

const boundary = vi.hoisted(() => ({
    snapshot: { active: true, epoch: 0, atMs: 0, departedAtMs: null } as AppLockLifecycle,
    policy: null as AppLockNativePolicy | null,
    visible: false,
    rejectedFrames: 1,
    listener: null as ((event: { payload: AppLockLifecycle }) => void) | null,
}))
vi.mock('@tauri-apps/api/core', async (original) => ({
    ...await original<typeof import('@tauri-apps/api/core')>(),
    invoke: async (command: string, args?: Record<string, unknown>) => {
        if (command === 'app_lock_check') return { auth: { status: 'unsupported' }, previewSupported: false }
        if (command === 'app_lock_set_policy') {
            boundary.policy = args!.policy as AppLockNativePolicy
            return boundary.snapshot
        }
        if (command === 'app_lock_ready') {
            if (args!.epoch !== boundary.snapshot.epoch) return false
            if (boundary.rejectedFrames-- > 0) return false
            boundary.policy!.gateRequired = args!.locked as boolean
            boundary.visible = true
            return true
        }
        throw new Error(`Unexpected native command: ${command}`)
    },
}))
vi.mock('@tauri-apps/api/event', async (original) => ({
    ...await original<typeof import('@tauri-apps/api/event')>(),
    listen: async (_event: string, listener: (event: { payload: AppLockLifecycle }) => void) => {
        boundary.listener = listener
        return () => { if (boundary.listener === listener) boundary.listener = null }
    },
}))

describe('app lock startup facade', () => {
    it('releases a boot failure before marker initialization and restores lock policy before normal content', async () => {
        appLock.setBackgroundColor('rgb(255 255 255)')
        await appLock.renderLoading()
        expect(appLock.state.initialized).toBe(false)
        expect(boundary.visible).toBe(true)
        expect(boundary.policy).toMatchObject({ gateRequired: false, enabled: false })
        boundary.visible = false
        await appLock.renderBootFailure()
        expect(appLock.state.initialized).toBe(false)
        expect(boundary.visible).toBe(true)
        expect(boundary.policy).toMatchObject({ gateRequired: false, enabled: false, backgroundColor: '#ffffff' })
        boundary.visible = false
        boundary.snapshot = { active: true, epoch: 2, atMs: 100, departedAtMs: 50 }
        boundary.listener!({ payload: boundary.snapshot })
        await vi.waitFor(() => expect(boundary.visible).toBe(true))
        expect(boundary.policy!.gateRequired).toBe(false)
        const pin = await createPinRecord('123456')
        const record = JSON.stringify({ ...defaultAppLockRecord(), enabled: true, pin })
        await appLock.initialize({ read: () => record, commit: async () => {} })
        expect(appLock.state.locked).toBe(true)
        expect(boundary.policy).toMatchObject({ gateRequired: true, enabled: true })
    })

    it('refreshes persisted lock data on read retry and retains effective state when the refresh fails', async () => {
        const pin = await createPinRecord('123456')
        const previous = { ...defaultAppLockRecord(), enabled: true, pin, timing: 'immediate' as const }
        await appLock.initialize({ read: () => JSON.stringify(previous), commit: async () => {} })
        await appLock.retryRead()
        let persisted = 'malformed'
        let failRead = false
        let wrote = false
        const backend: NativeDeviceSettings = {
            get: async () => persisted,
            readMany: async keys => {
                if (failRead) throw new Error('synthetic read failure')
                return keys.map(key => key === APP_LOCK_KEY ? persisted : null)
            },
            set: async () => { wrote = true },
            patch: async () => {},
        }
        const markers = createNativeDeviceMarkers(backend, await backend.readMany(DEVICE_MARKER_KEYS))
        installDeviceMarkers(markers)
        try {
            await appLock.initialize()
            await appLock.retryRead()
            expect(appLock.state.readFailed).toBe(true)
            expect(appLock.state.record).toEqual(previous)
            persisted = JSON.stringify({ ...previous, timing: 'after-5m' })
            failRead = true
            await appLock.retryRead()
            expect(appLock.state.readFailed).toBe(true)
            expect(appLock.state.record).toEqual(previous)
            expect(markers.getItem(APP_LOCK_KEY)).toBe('malformed')
            failRead = false
            await appLock.retryRead()
            expect(appLock.state.readFailed).toBe(false)
            expect(appLock.state.record.timing).toBe('after-5m')
            expect(appLock.state.locked).toBe(true)
            expect(markers.getItem(APP_LOCK_KEY)).toBe(persisted)
            expect(wrote).toBe(false)
        } finally { installDeviceMarkers(null) }
    })

    it('catches up a return whose event never reached the page when the page is shown again', async () => {
        const pin = await createPinRecord('123456')
        await appLock.initialize({ read: () => JSON.stringify({ ...defaultAppLockRecord(), enabled: true, pin }), commit: async () => {} })
        const epoch = appLock.state.epoch
        boundary.snapshot = { active: true, epoch: epoch + 2, atMs: 900, departedAtMs: 800 }
        window.dispatchEvent(new Event('focus'))
        await vi.waitFor(() => expect(appLock.state.epoch).toBe(epoch + 2))
        expect(appLock.state.returnPending).toBe(true)
    })
})
