// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const state = vi.hoisted(() => ({
    listProviders: vi.fn(),
    prepareConnection: vi.fn(),
    prepareConnectionSettingsImport: vi.fn(),
    commitConnection: vi.fn(),
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
        prepareConnectionSettingsImport: state.prepareConnectionSettingsImport,
        commitConnection: state.commitConnection,
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
        immutableCreate: true,
        directCompleteRead: true,
        atomicCreateHead: false,
        conditionalHeadUpdate: false,
        stableHeadReplace: true,
        headReadAfterWrite: true,
        headRetryControl: true,
        leaseOperations: true,
        deleteObjects: true,
        conditionalGet: false,
        resumableUpload: true,
        range: true,
        snapshotDiscovery: true,
        maxStoredBytes: null,
        sdkOverheadBytes: 0,
        uploadAlignment: 1,
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
    button(strings.prepare).click()
    await settle()
    labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
    await settle()
}

async function selectProvider(id: string): Promise<void> {
    const select = labelControl<HTMLSelectElement>(strings.provider)
    select.value = id
    select.dispatchEvent(new Event('change', { bubbles: true }))
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

describe('opening an existing repository', () => {
    it('authenticates imported connection settings with the recovery key before review', async () => {
        state.prepareConnectionSettingsImport.mockResolvedValue({
            ...prepared,
            endpoint: { ...prepared.endpoint, repositoryHint: 'recovered-folder' },
        })
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn(), restoreOnly: true },
        })
        await settle()

        expect(target.textContent).not.toContain(strings.create)
        expect([...target.querySelectorAll('label')]
            .some(item => item.textContent?.includes(strings.provider))).toBe(true)
        expect(target.textContent).toContain(strings.connectionSettingsImportHelp)

        const payload = target.querySelector('textarea')
        if (!payload) throw new Error('Missing connection settings payload field')
        payload.value = 'connection-settings-payload'
        payload.dispatchEvent(new Event('input', { bubbles: true }))
        const code = labelControl<HTMLInputElement>(strings.recoveryCode)
        code.value = 'recovery-code'
        code.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        button(strings.importConnectionSettings).click()
        await settle()

        expect(state.prepareConnectionSettingsImport)
            .toHaveBeenCalledWith('connection-settings-payload', 'recovery-code')
        expect(state.prepareConnection).not.toHaveBeenCalled()
        expect(target.textContent).toContain('recovered-folder')
        expect(target.textContent).not.toContain(strings.purposeReview)
    })

    it('supports manual provider setup with only the fixed recovery key', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn(), restoreOnly: true },
        })
        await settle()

        const code = labelControl<HTMLInputElement>(strings.recoveryCode)
        code.value = 'fixed-recovery-key'
        code.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        button(strings.prepare).click()
        await settle()

        expect(state.prepareConnection).toHaveBeenCalledWith(expect.objectContaining({
            mode: 'existing',
            recoveryKey: 'fixed-recovery-key',
        }))
        expect(state.prepareConnectionSettingsImport).not.toHaveBeenCalled()
    })
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
        callback.value = 'https://update.rsyumi.workers.dev/oauth/google-drive-callback?code=one&state=wrong'
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
            'https://update.rsyumi.workers.dev/oauth/google-drive-callback?code=one&state=wrong',
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

describe('service presets', () => {
    it('asks only WebDAV for the username used to authenticate', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('webdav')
        expect(labelControl<HTMLInputElement>('User name')).toBeDefined()
        for (const provider of ['s3', 'mybox', 'github_releases', 'gitlab_packages', 'google_drive', 'onedrive']) {
            await selectProvider(provider)
            const labels = [...target.querySelectorAll('label.field > span')].map(label => label.textContent)
            expect(labels).not.toContain('Account name')
            expect(labels).not.toContain('Token owner')
            expect(labels).not.toContain('GitHub user name')
            expect(labels.some(label => label === 'Account' || label?.startsWith('Account ('))).toBe(false)
        }
    })

    it('requires one GitLab access token with clear project permissions', async () => {
        state.prepareConnection.mockResolvedValue({
            ...prepared,
            endpoint: { ...prepared.endpoint, providerId: 'gitlab_packages' },
            requiresOAuth: false,
        })
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('gitlab_packages')
        button(strings.prepare).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        expect(target.textContent).toContain('api scope')
        expect(target.textContent).toContain('Maintainer')
        expect(target.textContent).not.toContain('Token type')
        expect(target.textContent).not.toContain('Deploy token')
    })

    it('offers Amazon S3 under its own name without becoming the preselected preset', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')

        const presets = labelControl<HTMLSelectElement>(strings.profile)
        expect([...presets.options].map(option => [option.value, option.textContent?.trim()]))
            .toContainEqual(['aws', 'Amazon S3'])
        expect(presets.value).toBe('r2')
    })
})

describe('synchronization mode defaults', () => {
    it('shows common single-device guidance without a strategy selector', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')

        button(strings.sync).click()
        await settle()

        expect(target.textContent).toContain(strings.sequentialWarning)
        expect(target.textContent).not.toContain('Concurrent-use protection')

        await selectProvider('google_drive')
        button(strings.sync).click()
        await settle()

        expect(target.textContent).toContain(strings.sequentialWarning)
        expect(target.querySelectorAll('input[type="checkbox"]')).toHaveLength(0)
    })
})

describe('what a connection stores', () => {
    it('offers the four backup items and no selection at all for synchronization', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')

        for (const item of [strings.hypa, strings.devicePlugins, strings.deviceSettings]) {
            expect(labelControl<HTMLInputElement>(item).type).toBe('checkbox')
        }
        // The library is always stored, so its row states the outcome instead
        // of offering a control that does nothing.
        expect(() => labelControl<HTMLInputElement>(strings.library)).toThrow()
        const library = [...target.querySelectorAll('p')]
            .find(item => item.textContent?.includes(strings.library))
        expect(library?.textContent).toContain(strings.included)
        expect(library?.querySelector('input')).toBeNull()

        button(strings.sync).click()
        await settle()

        // A synchronization connection has no selection screen; local data is
        // chosen per device instead.
        for (const item of [strings.hypa, strings.devicePlugins, strings.deviceSettings]) {
            expect(() => labelControl<HTMLInputElement>(item)).toThrow()
        }
        expect(target.textContent).not.toContain(strings.scopeHelp)
        expect(target.textContent).not.toContain(strings.included)
    })

    it('sends the chosen policy for a backup and none for a synchronization', async () => {
        state.prepareConnection.mockResolvedValue(prepared)
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')
        labelControl<HTMLInputElement>(strings.hypa).click()
        await settle()
        button(strings.prepare).click()
        await settle()

        expect(state.prepareConnection).toHaveBeenCalledWith(expect.objectContaining({
            purpose: 'backup',
            capturePolicy: { hypa: false, localPlugins: true, localSettings: true },
        }))

        state.prepareConnection.mockClear()
        button(strings.sync).click()
        await settle()
        button(strings.prepare).click()
        await settle()

        const request = state.prepareConnection.mock.calls.at(-1)?.[0]
        expect(request.purpose).toBe('sync')
        expect(request.capturePolicy).toBeUndefined()
        expect(request).not.toHaveProperty('publicationStrategy')
    })

    it('says where local data is chosen when reviewing a synchronization connection', async () => {
        state.prepareConnection.mockResolvedValue(prepared)
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')
        button(strings.sync).click()
        await settle()
        button(strings.prepare).click()
        await settle()

        expect(target.textContent).toContain(strings.syncPurposeLocalDataNotice)
    })
})

describe('native failure messages', () => {
    it('reports unsupported repositories without suggesting a strategy choice', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')
        button(strings.sync).click()
        await settle()

        state.prepareConnection.mockResolvedValueOnce({
            ...prepared,
            requiresOAuth: false,
            endpoint: { ...prepared.endpoint, providerId: 's3', authority: 's3.test' },
        })
        button(strings.prepare).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()

        state.commitConnection.mockRejectedValueOnce({ kind: 'unsupported', httpStatus: null, retryAtMs: null })
        button(strings.connect).click()
        await settle()

        expect(target.textContent).toContain(strings.unsupportedOperation)
    })

    it('reports a rejected sign-in instead of a bare failure', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await beginGoogleAuthorization()

        state.completeAuthorization.mockRejectedValueOnce({ kind: 'unauthorized', httpStatus: 401, retryAtMs: null })
        button(strings.finishSignIn).click()
        await settle()

        await vi.waitFor(() => expect(target.textContent).toContain(strings.credentialsRejected))
        expect(target.textContent).not.toContain(strings.unsupportedOperation)
    })
})
