// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const state = vi.hoisted(() => ({
    listProviders: vi.fn(),
    prepareConnection: vi.fn(),
    prepareRecoveryImport: vi.fn(),
    commitConnection: vi.fn(),
    beginAuthorization: vi.fn(),
    completeAuthorization: vi.fn(),
    cancelAuthorization: vi.fn(),
    openUrl: vi.fn(),
}))

vi.mock('src/ts/platform', () => ({
    isTauriAndroid: false,
    isTauriIOS: true,
}))
vi.mock('@tauri-apps/plugin-os', () => ({ type: () => 'ios' }))
vi.mock('@tauri-apps/plugin-opener', () => ({ openUrl: state.openUrl }))
vi.mock('src/ts/storage/sync/external/bridge', () => ({
    getExternalStorageBridge: () => ({
        listProviders: state.listProviders,
        prepareConnection: state.prepareConnection,
        prepareRecoveryImport: state.prepareRecoveryImport,
        commitConnection: state.commitConnection,
        beginAuthorization: state.beginAuthorization,
        completeAuthorization: state.completeAuthorization,
        cancelAuthorization: state.cancelAuthorization,
    }),
}))

import ConnectionForm from './ConnectionForm.svelte'
import { externalStorageStrings } from './strings'

const strings = externalStorageStrings('en')
let target: HTMLDivElement
let component: ReturnType<typeof mount> | undefined

function button(text: string): HTMLButtonElement {
    const result = [...target.querySelectorAll('button')]
        .find(item => item.textContent?.trim() === text)
    if (!result) throw new Error(`Missing button: ${text}`)
    return result
}

function labelControl<T extends HTMLInputElement>(text: string): T {
    const label = [...target.querySelectorAll('label')]
        .find(item => item.textContent?.includes(text))
    const control = label?.querySelector('input')
    if (!control) throw new Error(`Missing control: ${text}`)
    return control as T
}

async function settle(): Promise<void> {
    await tick()
    await Promise.resolve()
    await tick()
}

beforeEach(() => {
    vi.clearAllMocks()
    target = document.createElement('div')
    document.body.append(target)
    state.listProviders.mockResolvedValue([{
        id: 'google_drive', displayName: 'Google Drive', oauth: true,
        authorizationAvailable: true, strategies: ['sequential', 'backup-only'], profiles: ['drive'],
    }])
    state.prepareConnection.mockResolvedValue({
        preparationId: 'preparation-1',
        expiresAtMs: '1000',
        endpoint: {
            providerId: 'google_drive', authority: 'www.googleapis.com',
            repositoryHint: 'folder-1', warnings: [], remoteVerified: false,
        },
        capabilities: {
            immutableCreate: true, directCompleteRead: true, atomicCreateHead: false,
            conditionalHeadUpdate: false, stableHeadReplace: true, headReadAfterWrite: true,
            headRetryControl: true, leaseOperations: true, deleteObjects: true,
            conditionalGet: false, resumableUpload: true, range: true,
            snapshotDiscovery: true, maxStoredBytes: null, sdkOverheadBytes: 0,
            uploadAlignment: 1,
        },
        requiresOAuth: true,
        requiresRecoveryKey: false,
        requiresPlatformOAuthClient: false,
    })
    state.beginAuthorization.mockResolvedValue({
        authorizationId: 'authorization-1',
        expiresAtMs: '1000',
        state: 'complete',
    })
    state.completeAuthorization.mockResolvedValue({
        connection: { id: 'google-connection' },
    })
    state.cancelAuthorization.mockResolvedValue(undefined)
})

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    target.remove()
})

describe('iOS native authorization', () => {
    it('completes the connection after the native callback without another action', async () => {
        const onconnected = vi.fn()
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected, oncancel: vi.fn() },
        })
        await settle()

        button(strings.prepare).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        button(strings.signIn).click()

        await vi.waitFor(() => expect(onconnected).toHaveBeenCalledWith({
            connection: { id: 'google-connection' },
        }))
        expect(state.openUrl).not.toHaveBeenCalled()
        expect(state.completeAuthorization)
            .toHaveBeenCalledExactlyOnceWith('authorization-1', undefined, undefined)
        expect(target.textContent).not.toContain(strings.finishSignIn)
        expect(target.textContent).not.toContain(strings.manualOAuthCallback)
    })
})
