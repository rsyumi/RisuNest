// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const state = vi.hoisted(() => ({
    listProviders: vi.fn(),
    prepareConnection: vi.fn(),
    beginAuthorization: vi.fn(),
    completeAuthorization: vi.fn(),
    cancelAuthorization: vi.fn(),
    openUrl: vi.fn(),
}))

vi.mock('src/ts/platform', () => ({
    isTauriAndroid: true,
    isTauriIOS: false,
}))
vi.mock('@tauri-apps/plugin-os', () => ({ type: () => 'android' }))
vi.mock('@tauri-apps/plugin-opener', () => ({ openUrl: state.openUrl }))
vi.mock('src/ts/storage/sync/external/bridge', () => ({
    getExternalStorageBridge: () => ({
        listProviders: state.listProviders,
        prepareConnection: state.prepareConnection,
        beginAuthorization: state.beginAuthorization,
        completeAuthorization: state.completeAuthorization,
        cancelAuthorization: state.cancelAuthorization,
    }),
}))

import ConnectionForm from './ConnectionForm.svelte'
import { externalStorageStrings } from './strings'

let target: HTMLDivElement
let component: ReturnType<typeof mount> | undefined
const strings = externalStorageStrings('en')
const prepared = {
    preparationId: 'preparation-1',
    expiresAtMs: '1000' as const,
    endpoint: {
        providerId: 'google_drive' as const,
        authority: 'www.googleapis.com',
        repositoryHint: 'folder-1',
        warnings: [],
        remoteVerified: false,
    },
    capabilities: {
        cas: false,
        sequential: true,
        backupOnly: true,
        resumableUpload: true,
        rangeDownload: true,
        snapshotDiscovery: true,
        evidence: 'live' as const,
    },
    requiresOAuth: true,
    requiresRecoveryKey: false,
    requiresPlatformOAuthClient: false,
}

function labelControl<T extends HTMLInputElement | HTMLSelectElement>(text: string): T {
    const label = [...target.querySelectorAll('label')].find(item => item.textContent?.includes(text))
    const control = label?.querySelector('input, select')
    if (!control) throw new Error(`Missing control: ${text}`)
    return control as T
}

function button(text: string): HTMLButtonElement {
    const result = [...target.querySelectorAll('button')].find(item => item.textContent?.trim() === text)
    if (!result) throw new Error(`Missing button: ${text}`)
    return result
}

function formFieldset(): HTMLFieldSetElement {
    const result = target.querySelector<HTMLFieldSetElement>('[data-external-storage-connection-form]')
    if (!result) throw new Error('Missing connection form fieldset')
    return result
}

async function settle(): Promise<void> {
    await tick()
    await Promise.resolve()
    await tick()
}

async function prepareGoogleConnection(): Promise<void> {
    labelControl<HTMLInputElement>(strings.backupWarning).click()
    await settle()
    button(strings.prepare).click()
    await settle()
    labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
    await settle()
}

async function beginGoogleAuthorization(): Promise<void> {
    await prepareGoogleConnection()
    button(strings.signIn).click()
    await settle()
}

beforeEach(() => {
    vi.clearAllMocks()
    target = document.createElement('div')
    document.body.append(target)
    state.listProviders.mockResolvedValue([{
        id: 'google_drive', displayName: 'Google Drive', oauth: true,
        authorizationAvailable: true, strategies: ['sequential', 'backup-only'], profiles: ['drive'],
    }])
    state.prepareConnection.mockResolvedValue(prepared)
    state.beginAuthorization.mockResolvedValue({
        authorizationId: 'authorization-1',
        authorizationUrl: 'https://accounts.google.test/authorize',
        expiresAtMs: '1000',
        state: 'browser-required',
    })
    state.cancelAuthorization.mockResolvedValue(undefined)
})

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    target.remove()
})

describe('Android Google authorization lifecycle', () => {
    it('keeps a rejected pasted callback editable, then locks parent cancel through successful completion', async () => {
        const onconnected = vi.fn()
        const busyStates: boolean[] = []
        component = mount(ConnectionForm, {
            target,
            props: {
                strings,
                onconnected,
                oncancel: vi.fn(),
                onbusychange: (busy: boolean) => busyStates.push(busy),
            },
        })
        await settle()
        await beginGoogleAuthorization()

        const callback = labelControl<HTMLInputElement>(strings.manualOAuthCallback)
        callback.value = 'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html?code=one&state=wrong'
        callback.dispatchEvent(new Event('input', { bubbles: true }))
        state.completeAuthorization.mockResolvedValueOnce({
            authorizationPending: true,
            callbackRejected: true,
        })
        button(strings.finishSignIn).click()
        await settle()

        expect(target.textContent).toContain(strings.callbackRejected)
        expect(labelControl<HTMLInputElement>(strings.manualOAuthCallback).value).toContain('state=wrong')
        expect(state.cancelAuthorization).not.toHaveBeenCalled()

        let finishCompletion!: (value: unknown) => void
        state.completeAuthorization.mockImplementationOnce(() => new Promise(resolve => {
            finishCompletion = resolve
        }))
        button(strings.finishSignIn).click()
        await tick()

        expect(formFieldset().disabled).toBe(true)
        expect(busyStates.at(-1)).toBe(true)
        finishCompletion({ connection: { id: 'google-connection' } })
        await vi.waitFor(() => expect(onconnected).toHaveBeenCalled())
        await tick()

        expect(onconnected).toHaveBeenCalledWith({ connection: { id: 'google-connection' } })
        expect(state.completeAuthorization).toHaveBeenLastCalledWith(
            'authorization-1',
            'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html?code=one&state=wrong',
            undefined,
        )
        await vi.waitFor(() => expect(busyStates.at(-1)).toBe(false))
    })

    it('awaits native cancellation before resetting a pending attempt', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await beginGoogleAuthorization()

        let finishCancel!: () => void
        state.cancelAuthorization.mockImplementationOnce(() => new Promise<void>(resolve => {
            finishCancel = resolve
        }))
        button(strings.back).click()
        await tick()

        expect(state.cancelAuthorization).toHaveBeenCalledWith('authorization-1')
        expect(target.textContent).toContain(strings.endpointReview)
        expect(formFieldset().disabled).toBe(true)

        finishCancel()
        await vi.waitFor(() => expect(target.textContent).not.toContain(strings.endpointReview))
        await tick()

        expect(target.textContent).not.toContain(strings.endpointReview)
        expect(button(strings.prepare)).toBeDefined()
    })

    it('cancels an idle pending authorization when the form unmounts', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await beginGoogleAuthorization()

        await unmount(component)
        component = undefined
        await Promise.resolve()

        expect(state.cancelAuthorization).toHaveBeenCalledExactlyOnceWith('authorization-1')
    })

    it('cancels an authorization ID that arrives after the form unmounts', async () => {
        let finishBegin!: (value: unknown) => void
        state.beginAuthorization.mockImplementationOnce(() => new Promise(resolve => {
            finishBegin = resolve
        }))
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await prepareGoogleConnection()
        button(strings.signIn).click()
        await tick()

        await unmount(component)
        component = undefined
        finishBegin({
            authorizationId: 'late-authorization',
            authorizationUrl: 'https://accounts.google.test/authorize',
            expiresAtMs: '1000',
            state: 'browser-required',
        })
        await Promise.resolve()
        await Promise.resolve()

        expect(state.cancelAuthorization).toHaveBeenCalledExactlyOnceWith('late-authorization')
        expect(state.openUrl).not.toHaveBeenCalled()
    })
})
