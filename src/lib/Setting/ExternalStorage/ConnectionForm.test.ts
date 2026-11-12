// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const state = vi.hoisted(() => ({
    native: true,
    validateSyncRoot: vi.fn(async () => {}),
    listProviders: vi.fn(),
    prepareConnection: vi.fn(),
    prepareRenewal: vi.fn(),
    changeEndpoint: vi.fn(),
    prepareConnectionSettingsImport: vi.fn(),
    commitConnection: vi.fn(),
    beginAuthorization: vi.fn(),
    completeAuthorization: vi.fn(),
    cancelAuthorization: vi.fn(),
    listFolders: vi.fn(),
    selectFolder: vi.fn(),
    cancelFolderSelection: vi.fn(),
    openUrl: vi.fn(),
    onAuthorizationReady: vi.fn(),
    stopAuthorizationListener: vi.fn(),
}))

const platformState = vi.hoisted(() => ({ android: true, ios: false }))
const qr = vi.hoisted(() => ({ scan: vi.fn(), cancel: vi.fn() }))
vi.mock('src/ts/ui/qrScanner', async importOriginal => ({
    ...await importOriginal<typeof import('src/ts/ui/qrScanner')>(),
    createQrScanner: () => qr,
}))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule(() => ({
    runtime: 'tauri', os: platformState.ios ? 'ios' : platformState.android ? 'android' : 'windows',
})))
vi.mock('@tauri-apps/plugin-os', () => ({ type: () => 'android' }))
vi.mock('@tauri-apps/plugin-opener', () => ({ openUrl: state.openUrl }))
vi.mock('src/ts/storage/sync/external/bridge', () => ({
    getExternalStorageBridge: () => ({
        get supported() { return state.native },
        validateSyncRoot: state.validateSyncRoot,
        listProviders: state.listProviders,
        prepareConnection: state.prepareConnection,
        prepareRenewal: state.prepareRenewal,
        changeEndpoint: state.changeEndpoint,
        prepareConnectionSettingsImport: state.prepareConnectionSettingsImport,
        commitConnection: state.commitConnection,
        beginAuthorization: state.beginAuthorization,
        completeAuthorization: state.completeAuthorization,
        cancelAuthorization: state.cancelAuthorization,
        onAuthorizationReady: state.onAuthorizationReady,
        listFolders: state.listFolders,
        selectFolder: state.selectFolder,
        cancelFolderSelection: state.cancelFolderSelection,
    }),
}))

import ConnectionForm from './ConnectionForm.svelte'
import { externalConnectionPlace, externalConnectionTitle, externalStorageStrings } from './strings'
import { QrScanError } from 'src/ts/ui/qrScanner'

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
    requiresFolderSelection: false,
}
const preparedForSelection = {
    ...prepared,
    endpoint: { ...prepared.endpoint, repositoryHint: '' },
    requiresFolderSelection: true,
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

function folderRow(): HTMLElement {
    const result = target.querySelector<HTMLElement>('[data-folder-row]')
    if (!result) throw new Error('Missing folder row')
    return result
}

function folderAction(): HTMLButtonElement {
    const result = folderRow().querySelector<HTMLButtonElement>('[data-folder-action]')
    if (!result) throw new Error('Missing folder action')
    return result
}

async function selectMode(label: string): Promise<void> {
    const control = [...target.querySelectorAll<HTMLButtonElement>('[role="radio"]')]
        .find(item => item.textContent?.trim() === label)
    if (!control) throw new Error(`Missing mode: ${label}`)
    control.click()
    await settle()
}

function typeInto(control: HTMLInputElement, value: string): void {
    control.value = value
    control.dispatchEvent(new Event('input', { bubbles: true }))
}

async function settle(): Promise<void> {
    await tick()
    await new Promise(resolve => setTimeout(resolve, 0))
    await tick()
}

/** Fills the required fields a test leaves empty, as a user would before continuing. */
async function fillRequiredFields(): Promise<void> {
    for (const label of target.querySelectorAll('label')) {
        const input = label.querySelector<HTMLInputElement>('input[id^="external-storage-field-"]')
        if (!input || input.value.trim() || label.querySelector('span')?.textContent?.endsWith(strings.optional)) continue
        typeInto(input, input.id.endsWith('-endpoint') ? 'https://storage.test'
            : input.type === 'datetime-local' ? '2030-01-01T00:00' : 'synthetic-value')
        input.dispatchEvent(new Event('change', { bubbles: true }))
    }
    await settle()
}

async function prepareGoogleConnection(): Promise<void> {
    await fillRequiredFields()
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
    platformState.android = true
    platformState.ios = false
    state.native = true
    for (const mock of Object.values(state)) if (typeof mock !== 'boolean') mock.mockReset()
    qr.scan.mockReset()
    qr.cancel.mockReset()
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
    state.onAuthorizationReady.mockResolvedValue(state.stopAuthorizationListener)
    state.cancelFolderSelection.mockResolvedValue(undefined)
    state.listFolders.mockResolvedValue({ path: [], folders: [], selectable: false })
})

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    target.remove()
})

describe('choosing a service', () => {
    it('names the service once, as the label of its select', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()

        const headings = [...target.querySelectorAll('h1, h2, h3, h4, h5, h6')].map(item => item.textContent?.trim())
        expect(headings).not.toContain(strings.provider)
        const named = [...target.querySelectorAll('*')]
            .filter(item => item.children.length === 0 && item.textContent?.trim() === strings.provider)
        expect(named).toHaveLength(1)
        expect(named[0].closest('label')?.querySelector('select')).not.toBeNull()
    })

    it('warns about the hidden app data space only while it is the chosen Google Drive location', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        const google = strings.providers.google_drive
        const space = labelControl<HTMLSelectElement>(strings.fields['google_drive.space'])
        expect(space.value).toBe('drive')
        expect(target.textContent).not.toContain(google.warningTitle)
        expect(target.textContent).not.toContain(google.warning)

        space.value = 'appDataFolder'
        space.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        expect(target.textContent).toContain(google.warningTitle)
        expect(target.textContent).toContain(google.warning)

        const visible = labelControl<HTMLSelectElement>(strings.fields['google_drive.space'])
        visible.value = 'drive'
        visible.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        expect(target.textContent).not.toContain(google.warningTitle)
    })

    it('keeps the GitLab cleanup policy warning for every GitLab connection', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectProvider('gitlab_packages')
        expect(target.textContent).toContain(strings.providers.gitlab_packages.warningTitle)
    })
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
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()

        expect(state.prepareConnection).toHaveBeenCalledWith(expect.objectContaining({
            mode: 'existing',
            recoveryKey: 'fixed-recovery-key',
        }))
        expect(state.prepareConnectionSettingsImport).not.toHaveBeenCalled()
    })
})

describe('scanning connection settings', () => {
    const payload = () => target.querySelector<HTMLTextAreaElement>('textarea')!.value
    const restore = (tone?: 'onboarding') => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn(), restoreOnly: true, ...(tone ? { tone } : {}) },
        })
    }

    it('fills the settings field from the scanned code in the screen wording', async () => {
        qr.scan.mockResolvedValue('  scanned-settings  ')
        restore()
        await settle()
        button(strings.scanConnectionSettings).click()
        await settle()
        expect(qr.scan).toHaveBeenCalledExactlyOnceWith('settings')
        expect(payload()).toBe('scanned-settings')
        expect(target.querySelector('[role="alert"]')).toBeNull()
    })

    it('uses the onboarding wording from the onboarding screen', async () => {
        qr.scan.mockResolvedValue('scanned-settings')
        restore('onboarding')
        await settle()
        button(strings.scanConnectionSettings).click()
        await settle()
        expect(qr.scan).toHaveBeenCalledExactlyOnceWith('onboarding')
    })

    it('returns quietly from a cancelled scan and reports a failed one', async () => {
        qr.scan.mockRejectedValueOnce(new QrScanError('qr-scan-cancelled')).mockRejectedValueOnce(new QrScanError('qr-camera-permission-denied'))
        restore()
        await settle()
        button(strings.scanConnectionSettings).click()
        await settle()
        expect(target.querySelector('[role="alert"]')).toBeNull()
        expect(payload()).toBe('')
        button(strings.scanConnectionSettings).click()
        await settle()
        expect(target.querySelector('[role="alert"]')?.textContent).toBe(strings.cameraDenied)
    })

    it('marks the scan button busy while the camera runs and ends the scan when the form closes', async () => {
        qr.scan.mockReturnValue(new Promise(() => {}))
        restore()
        await settle()
        button(strings.scanConnectionSettings).click()
        await settle()
        const scanButton = [...target.querySelectorAll('button')].find(item => item.textContent?.includes(strings.scanConnectionSettings))
        expect(scanButton?.getAttribute('aria-busy')).toBe('true')
        await unmount(component!)
        component = undefined
        expect(qr.cancel).toHaveBeenCalledOnce()
    })
})

describe('OAuth folder setup', () => {
    it('uses a folder name for create mode and never renders provider ID fields', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()

        expect(labelControl<HTMLInputElement>(strings.fields['google_drive.folderName']).value).toBe('RisuNest')
        expect(target.textContent).not.toContain('Folder ID')
        expect(target.textContent).not.toContain('Drive ID')
        expect(target.textContent).not.toContain('Folder item ID')
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()

        expect(state.prepareConnection).toHaveBeenCalledWith(expect.objectContaining({
            config: expect.objectContaining({
                location: expect.objectContaining({ folderName: 'RisuNest' }),
            }),
        }))
        expect(state.prepareConnection.mock.calls[0][0].config.location).not.toHaveProperty('folderId')
    })

    it('validates an empty folder name beside the field without preparing', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        const input = labelControl<HTMLInputElement>(strings.fields['google_drive.folderName'])
        typeInto(input, '   ')
        input.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        button(strings.prepare).click()
        await settle()

        expect(state.prepareConnection).not.toHaveBeenCalled()
        expect(input.closest('label')?.querySelector('[role="alert"]')?.textContent).toBe(strings.folderNameRequired)
        expect(document.activeElement).toBe(input)
    })

    it('hides folder creation input for hidden app data and sends no folder name', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        const space = labelControl<HTMLSelectElement>(strings.fields['google_drive.space'])
        space.value = 'appDataFolder'
        space.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        expect(() => labelControl(strings.fields['google_drive.folderName'])).toThrow()
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        expect(state.prepareConnection.mock.calls[0][0].config.location).not.toHaveProperty('folderName')
    })

    it('selects a Google folder and connects without exposing its provider ID', async () => {
        state.prepareConnection.mockResolvedValue(preparedForSelection)
        state.completeAuthorization.mockResolvedValue({
            folderSelected: true,
            folder: { name: 'Backups', accountHint: 'user@example.test' },
        })
        state.commitConnection.mockResolvedValue({ connection: { id: 'google-connection' } })
        const onconnected = vi.fn()
        component = mount(ConnectionForm, { target, props: { strings, onconnected, oncancel: vi.fn() } })
        await settle()
        await selectMode(strings.existing)
        typeInto(labelControl<HTMLInputElement>(strings.recoveryCode), 'recovery-key')
        await settle()
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()

        expect(button(strings.connect).disabled).toBe(true)
        button(strings.selectFolder).click()
        await settle()
        expect(state.beginAuthorization).toHaveBeenCalledWith('preparation-1', undefined)
        expect(state.openUrl).toHaveBeenCalled()
        expect(folderRow().dataset.state).toBe('selecting')
        expect(folderRow().textContent).toContain(strings.selectingFolder)
        button(strings.finishSignIn).click()
        await vi.waitFor(() => expect(folderRow().dataset.state).toBe('selected'))
        await settle()

        expect(folderRow().dataset.state).toBe('selected')
        expect(folderRow().textContent).toContain('Backups')
        expect([...target.querySelectorAll('input')].some(input => input.value.includes('Backups'))).toBe(false)
        expect(target.textContent).toContain('user@example.test')
        await vi.waitFor(() => expect(document.activeElement).toBe(folderAction()))
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        button(strings.connect).click()
        await vi.waitFor(() => expect(onconnected).toHaveBeenCalled())
        expect(state.commitConnection).toHaveBeenCalledWith('preparation-1')
    })

    it('restores a previous folder after a cancelled reselection', async () => {
        state.prepareConnection.mockResolvedValue(preparedForSelection)
        state.completeAuthorization
            .mockResolvedValueOnce({ folderSelected: true, folder: { name: 'Backups' } })
            .mockResolvedValueOnce({ folderSelectionCancelled: true })
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectMode(strings.existing)
        typeInto(labelControl<HTMLInputElement>(strings.recoveryCode), 'recovery-key')
        await settle()
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        button(strings.selectFolder).click()
        await settle()
        button(strings.finishSignIn).click()
        await vi.waitFor(() => expect(folderRow().dataset.state).toBe('selected'))
        await settle()
        button(strings.selectFolderAgain).click()
        await settle()
        button(strings.finishSignIn).click()
        await settle()

        expect(folderRow().dataset.state).toBe('selected')
        expect(folderRow().textContent).toContain('Backups')
        expect(folderRow().querySelector('[role="alert"]')).toBeNull()
    })

    it('keeps the review and requests another folder after repository validation fails', async () => {
        state.prepareConnection.mockResolvedValue(preparedForSelection)
        state.completeAuthorization.mockResolvedValue({ folderSelected: true, folder: { name: 'Empty' } })
        state.commitConnection.mockRejectedValue({ kind: 'folderNotRepository' })
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectMode(strings.existing)
        typeInto(labelControl<HTMLInputElement>(strings.recoveryCode), 'recovery-key')
        await settle()
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        typeInto(labelControl<HTMLInputElement>(strings.oauthClientSecret), 'secret')
        button(strings.selectFolder).click()
        await settle()
        button(strings.finishSignIn).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        button(strings.connect).click()
        await settle()

        expect(folderRow().dataset.state).toBe('invalid')
        expect(folderRow().textContent).toContain(strings.folderNotRepository)
        await vi.waitFor(() => expect(button(strings.selectFolder)).toBe(document.activeElement))
        expect(labelControl<HTMLInputElement>(strings.oauthClientSecret).value).toBe('secret')
        expect(target.textContent).toContain(strings.endpointReview)
    })

    it('opens the native-backed selector and releases its session on unmount', async () => {
        state.prepareConnection.mockResolvedValue(preparedForSelection)
        state.completeAuthorization.mockResolvedValue({
            folderSelectionRequired: true,
            selectionId: 'selection-1',
            accountHint: 'user@example.test',
        })
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectMode(strings.existing)
        typeInto(labelControl<HTMLInputElement>(strings.recoveryCode), 'recovery-key')
        await settle()
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        button(strings.selectFolder).click()
        await settle()
        button(strings.finishSignIn).click()
        await settle()

        expect(target.querySelector('[data-external-folder-selector]')).not.toBeNull()
        expect(state.listFolders).toHaveBeenCalledWith({ selectionId: 'selection-1' })
        await unmount(component)
        component = undefined
        await Promise.resolve()
        expect(state.cancelFolderSelection).toHaveBeenCalledExactlyOnceWith('selection-1')
    })

    it('returns to the OneDrive folder name after a same-name conflict', async () => {
        state.prepareConnection.mockResolvedValue({
            ...prepared,
            endpoint: { ...prepared.endpoint, providerId: 'onedrive', repositoryHint: 'RisuNest' },
        })
        state.completeAuthorization.mockRejectedValue({ kind: 'folderNameConflict' })
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectProvider('onedrive')
        const project = labelControl<HTMLInputElement>(strings.fields['onedrive.projectId'])
        project.value = 'application-id'
        project.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        button(strings.signIn).click()
        await settle()
        button(strings.finishSignIn).click()
        await vi.waitFor(() => expect(target.textContent).not.toContain(strings.endpointReview))
        await settle()

        const folderName = labelControl<HTMLInputElement>(strings.fields['onedrive.folderName'])
        expect(target.textContent).not.toContain(strings.endpointReview)
        expect(folderName.closest('label')?.textContent).toContain(strings.folderNameConflict)
        expect(labelControl<HTMLInputElement>(strings.fields['onedrive.projectId']).value).toBe('application-id')
        expect(document.activeElement).toBe(folderName)
    })

    it('switches an inaccessible imported folder to reselection without losing the review', async () => {
        state.prepareConnectionSettingsImport.mockResolvedValue(prepared)
        state.completeAuthorization.mockRejectedValue({ kind: 'folderInaccessible' })
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn(), restoreOnly: true },
        })
        await settle()
        const payload = target.querySelector<HTMLTextAreaElement>('textarea')!
        payload.value = 'settings'
        payload.dispatchEvent(new Event('input', { bubbles: true }))
        typeInto(labelControl<HTMLInputElement>(strings.recoveryCode), 'recovery-key')
        await settle()
        button(strings.importConnectionSettings).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        button(strings.signIn).click()
        await settle()
        button(strings.finishSignIn).click()
        await vi.waitFor(() => expect(target.querySelector('[data-folder-row]')).not.toBeNull())
        await settle()

        expect(folderRow().dataset.state).toBe('invalid')
        expect(folderRow().textContent).toContain(strings.folderInaccessible)
        expect(() => button(strings.signIn)).toThrow()
        expect(button(strings.connect).disabled).toBe(true)
        expect(target.textContent).toContain(strings.endpointReview)
    })
})

describe('Android Google authorization lifecycle', () => {
    it('finishes folder selection on callback without connecting an unconfirmed folder', async () => {
        state.prepareConnection.mockResolvedValue(preparedForSelection)
        state.completeAuthorization.mockResolvedValue({ folderSelected: true, folder: { name: 'Backups' } })
        const onconnected = vi.fn()
        component = mount(ConnectionForm, { target, props: { strings, onconnected, oncancel: vi.fn() } })
        await settle()
        await selectMode(strings.existing)
        typeInto(labelControl<HTMLInputElement>(strings.recoveryCode), 'recovery-key')
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        button(strings.selectFolder).click()
        await vi.waitFor(() => expect(state.openUrl).toHaveBeenCalled())
        state.onAuthorizationReady.mock.calls[0][0]('authorization-1')
        await vi.waitFor(() => expect(folderRow().dataset.state).toBe('selected'))
        expect(state.completeAuthorization).toHaveBeenCalledTimes(1)
        expect(onconnected).not.toHaveBeenCalled()
        expect(state.commitConnection).not.toHaveBeenCalled()
        expect(labelControl<HTMLInputElement>(strings.confirmEndpoint).checked).toBe(false)
    })

    it('releases a late listener without starting authorization after unmount', async () => {
        let register!: (stop: () => void) => void
        state.onAuthorizationReady.mockImplementationOnce(() => new Promise(resolve => { register = resolve }))
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await prepareGoogleConnection()
        button(strings.signIn).click()
        await vi.waitFor(() => expect(state.onAuthorizationReady).toHaveBeenCalled())
        await unmount(component)
        component = undefined
        register(state.stopAuthorizationListener)
        await settle()
        expect(state.stopAuthorizationListener).toHaveBeenCalledOnce()
        expect(state.beginAuthorization).not.toHaveBeenCalled()
    })

    it('automatically completes a matching callback once, including a callback during browser launch', async () => {
        const onconnected = vi.fn()
        component = mount(ConnectionForm, { target, props: { strings, onconnected, oncancel: vi.fn() } })
        await settle()
        state.completeAuthorization.mockResolvedValue({ connection: { id: 'google-connection' } })
        state.openUrl.mockImplementation(async () => {
            const ready = state.onAuthorizationReady.mock.calls[0]?.[0]
            ready?.('unrelated-authorization')
            ready?.('authorization-1')
            ready?.('authorization-1')
        })
        await beginGoogleAuthorization()
        await vi.waitFor(() => expect(onconnected).toHaveBeenCalledTimes(1))
        expect(state.completeAuthorization).toHaveBeenCalledExactlyOnceWith('authorization-1', undefined, undefined)
    })

    it('ignores stale callbacks and disposes the listener when the form closes', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await beginGoogleAuthorization()
        expect(state.onAuthorizationReady).toHaveBeenCalledTimes(1)
        const ready = state.onAuthorizationReady.mock.calls[0][0]
        ready('unrelated-authorization')
        await settle()
        expect(state.completeAuthorization).not.toHaveBeenCalled()
        await unmount(component)
        component = undefined
        expect(state.stopAuthorizationListener).toHaveBeenCalledTimes(1)
        ready('authorization-1')
        await settle()
        expect(state.completeAuthorization).not.toHaveBeenCalled()
    })

    it('does not repeat completion when the callback arrives during manual completion', async () => {
        let finish!: (value: unknown) => void
        state.completeAuthorization.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        const onconnected = vi.fn()
        component = mount(ConnectionForm, { target, props: { strings, onconnected, oncancel: vi.fn() } })
        await settle()
        await beginGoogleAuthorization()
        button(strings.finishSignIn).click()
        await settle()
        state.onAuthorizationReady.mock.calls[0][0]('authorization-1')
        await settle()
        expect(state.completeAuthorization).toHaveBeenCalledTimes(1)
        finish({ connection: { id: 'google-connection' } })
        await settle()
        expect(onconnected).toHaveBeenCalledTimes(1)
        expect(state.completeAuthorization).toHaveBeenCalledTimes(1)
    })

    it('reports a failed automatic completion and allows a fresh sign-in', async () => {
        state.completeAuthorization.mockRejectedValueOnce({ kind: 'reauthRequired' })
        const onconnected = vi.fn()
        component = mount(ConnectionForm, { target, props: { strings, onconnected, oncancel: vi.fn() } })
        await settle()
        await beginGoogleAuthorization()
        state.onAuthorizationReady.mock.calls[0][0]('authorization-1')
        await settle()
        expect(state.cancelAuthorization).toHaveBeenCalledExactlyOnceWith('authorization-1')
        expect(onconnected).not.toHaveBeenCalled()
        expect(target.querySelector('[role="alert"]')?.textContent).toContain(strings.reauthenticate)
        button(strings.signIn).click()
        await settle()
        expect(state.beginAuthorization).toHaveBeenCalledTimes(2)
        expect(state.onAuthorizationReady).toHaveBeenCalledTimes(1)
    })

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
        await prepareGoogleConnection()
        const clientSecret = labelControl<HTMLInputElement>(strings.oauthClientSecret)
        expect(target.textContent).not.toContain(strings.renewalClientSecretHelp)
        clientSecret.value = 'browser-client-secret'
        clientSecret.dispatchEvent(new Event('input', { bubbles: true }))
        button(strings.signIn).click()
        await settle()

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
            'browser-client-secret',
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
        await settle()
        expect(state.beginAuthorization).toHaveBeenCalledOnce()

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
        await fillRequiredFields()
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

describe('what a connection stores', () => {
    it.each(['webdav', 's3', 'google_drive', 'onedrive'])('shows native Sync purpose for %s', async id => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle(); await selectProvider(id)
        expect([...target.querySelectorAll('button')].some(item => item.textContent?.trim() === strings.sync)).toBe(true)
    })
    it.each(['github_releases', 'gitlab_packages', 'mybox'])('keeps %s backup only', async id => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle(); await selectProvider(id)
        expect([...target.querySelectorAll('button')].some(item => item.textContent?.trim() === strings.sync)).toBe(false)
    })
    it.each(['webdav', 's3', 'google_drive', 'onedrive'])('hides Sync purpose for %s on web', async id => {
        state.native = false
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle(); await selectProvider(id)
        expect([...target.querySelectorAll('button')].some(item => item.textContent?.trim() === strings.sync)).toBe(false)
    })
    it('calls the native Sync-root validator before preparing a Sync connection', async () => {
        state.validateSyncRoot.mockRejectedValueOnce({ kind: 'invalid-config' })
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle(); await selectProvider('s3'); button(strings.sync).click(); await settle()
        await fillRequiredFields()
        button(strings.prepare).click(); await settle()
        expect(state.validateSyncRoot).toHaveBeenCalledOnce()
        expect(state.prepareConnection).not.toHaveBeenCalled()
    })

    it('offers fixed backup scope and synchronization for an eligible provider', async () => {
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')

        for (const item of [strings.hypa, strings.devicePlugins, strings.deviceSettings]) {
            expect(() => labelControl<HTMLInputElement>(item)).toThrow()
        }
        expect(() => labelControl<HTMLInputElement>(strings.library)).toThrow()
        expect([...target.querySelectorAll('button')].some(button => button.textContent?.trim() === strings.sync)).toBe(true)
    })

    it('offers fixed full backups without exclusion controls', async () => {
        state.prepareConnection.mockResolvedValue(prepared)
        component = mount(ConnectionForm, {
            target,
            props: { strings, onconnected: vi.fn(), oncancel: vi.fn() },
        })
        await settle()
        await selectProvider('s3')
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()

        expect(state.prepareConnection).toHaveBeenCalledWith(expect.objectContaining({
            purpose: 'backup',
        }))

        expect(state.prepareConnection.mock.calls.at(-1)?.[0]).not.toHaveProperty('capturePolicy')
        expect(state.prepareConnection.mock.calls.at(-1)?.[0]).not.toHaveProperty('publicationStrategy')
    })

})

describe('required connection fields', () => {
    function changeValue(control: HTMLInputElement, value: string): void {
        typeInto(control, value)
        control.dispatchEvent(new Event('change', { bubbles: true }))
    }

    it('prepares and connects WebDAV with both credential fields left empty', async () => {
        state.prepareConnection.mockResolvedValue({ ...prepared, requiresOAuth: false,
            endpoint: { ...prepared.endpoint, providerId: 'webdav' } })
        state.commitConnection.mockResolvedValue({ connection: { id: 'anonymous' } })
        const connected = vi.fn()
        component = mount(ConnectionForm, { target, props: { strings, onconnected: connected, oncancel: vi.fn() } })
        await settle()
        await selectProvider('webdav')
        changeValue(labelControl<HTMLInputElement>(strings.endpoint), 'http://nas.test/dav')
        changeValue(labelControl<HTMLInputElement>(strings.fields['webdav.root']), 'RisuNest')
        await settle()
        button(strings.prepare).click()
        await settle()
        expect(state.prepareConnection).toHaveBeenCalledWith(expect.objectContaining({
            config: expect.objectContaining({ accountId: '', endpoint: 'http://nas.test/dav' }),
        }))
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        button(strings.connect).click()
        await settle()
        expect(state.commitConnection).toHaveBeenCalledWith('preparation-1', { kind: 'webdav', password: '' })
        expect(connected).toHaveBeenCalledWith({ connection: { id: 'anonymous' } })
        expect(target.querySelector('[role="alert"]')).toBeNull()
    })

    it('reports an empty WebDAV folder name beside the field without preparing', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectProvider('webdav')
        changeValue(labelControl<HTMLInputElement>(strings.endpoint), 'https://dav.example.test/remote.php/dav')
        changeValue(labelControl<HTMLInputElement>(strings.fields['webdav.accountId']), 'synthetic-user')
        await settle()
        button(strings.prepare).click()
        await settle()

        const root = labelControl<HTMLInputElement>(strings.fields['webdav.root'])
        expect(state.prepareConnection).not.toHaveBeenCalled()
        expect([...target.querySelectorAll('[role="alert"]')].map(alert => alert.textContent))
            .toEqual([strings.folderNameRequired])
        expect(root.closest('label')?.querySelector('[role="alert"]')).not.toBeNull()
        expect(document.activeElement).toBe(root)

        typeInto(root, 'RisuNest')
        await settle()
        expect(target.querySelector('[role="alert"]')).toBeNull()
        root.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        button(strings.prepare).click()
        await settle()
        expect(state.prepareConnection).toHaveBeenCalledWith(expect.objectContaining({
            config: expect.objectContaining({ accountId: 'synthetic-user', location: { root: 'RisuNest' } }),
        }))
    })

    it('describes the WebDAV folder as the one to create in or the one that holds the repository', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectProvider('webdav')
        const help = () => labelControl<HTMLInputElement>(strings.fields['webdav.root']).closest('label')?.textContent
        expect(help()).toContain(strings.fieldHelp['webdav.root'])
        await selectMode(strings.existing)
        expect(help()).toContain(strings.existingFieldHelp['webdav.root'])
        expect(help()).not.toContain(strings.fieldHelp['webdav.root'])
    })

    it('reports each empty required field and leaves optional fields alone', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectProvider('s3')
        button(strings.prepare).click()
        await settle()

        const alerts = [...target.querySelectorAll('[role="alert"]')]
        expect(alerts.map(alert => alert.closest('label')?.querySelector('span')?.textContent))
            .toEqual([strings.endpoint, strings.fields['s3.bucket']])
        expect(alerts.map(alert => alert.textContent)).toEqual([strings.fieldRequired, strings.fieldRequired])
        expect(document.activeElement).toBe(labelControl<HTMLInputElement>(strings.endpoint))
        expect(state.prepareConnection).not.toHaveBeenCalled()
    })

    it('reports an empty secret beside the field without connecting', async () => {
        state.prepareConnection.mockResolvedValue({
            ...prepared,
            requiresOAuth: false,
            endpoint: { ...prepared.endpoint, providerId: 's3', authority: 's3.test' },
        })
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        await selectProvider('s3')
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        typeInto(labelControl<HTMLInputElement>(strings.fields['s3.accessKeyId']), 'synthetic-access-key')
        await settle()
        button(strings.connect).click()
        await settle()

        const secret = labelControl<HTMLInputElement>(strings.fields['s3.secretAccessKey'])
        expect(state.commitConnection).not.toHaveBeenCalled()
        expect([...target.querySelectorAll('[role="alert"]')].map(alert => alert.textContent))
            .toEqual([strings.fieldRequired])
        expect(secret.closest('label')?.querySelector('[role="alert"]')).not.toBeNull()
        expect(document.activeElement).toBe(secret)
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

        state.prepareConnection.mockResolvedValueOnce({
            ...prepared,
            requiresOAuth: false,
            endpoint: { ...prepared.endpoint, providerId: 's3', authority: 's3.test' },
        })
        await fillRequiredFields()
        button(strings.prepare).click()
        await settle()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()

        state.commitConnection.mockRejectedValueOnce({ kind: 'unsupported', httpStatus: null, retryAtMs: null })
        await fillRequiredFields()
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

        state.completeAuthorization.mockRejectedValueOnce({
            kind: 'reauthRequired', httpStatus: 400, retryAtMs: null,
            oauthError: 'invalid_grant', oauthErrorDescription: 'Bad Request: redirect_uri is invalid.',
        })
        button(strings.finishSignIn).click()
        await settle()

        await vi.waitFor(() => {
            expect(target.textContent).toContain(strings.reauthenticate)
            expect(target.textContent).toContain(strings.oauthErrorCode.replace('{0}', 'invalid_grant'))
            expect(target.textContent).toContain(
                strings.oauthErrorDescription.replace('{0}', 'Bad Request: redirect_uri is invalid.'),
            )
        })
        expect(target.textContent).not.toContain(strings.unsupportedOperation)
    })
})

describe('authorization completion ownership', () => {
    it('can cancel an awaited completion and returns quietly to the prepared form', async () => {
        const connected = vi.fn()
        component = mount(ConnectionForm, { target, props: { strings, onconnected: connected, oncancel: vi.fn() } })
        await beginGoogleAuthorization()
        let rejectCompletion!: (reason: unknown) => void
        state.completeAuthorization.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectCompletion = reject }))
        button(strings.finishSignIn).click()
        await settle()
        expect(formFieldset().disabled).toBe(true)
        const cancel = [...target.querySelectorAll('button')].find(control =>
            control.textContent?.trim() === strings.cancel && !control.closest('fieldset'))!
        expect(cancel).toBeDefined()
        expect(cancel.disabled).toBe(false)
        cancel.click()
        await settle()
        expect(state.cancelAuthorization).toHaveBeenCalledWith('authorization-1')
        rejectCompletion({ kind: 'cancelled' })
        await vi.waitFor(() => expect(formFieldset().disabled).toBe(false))
        expect(state.cancelAuthorization).toHaveBeenCalledOnce()
        expect(connected).not.toHaveBeenCalled()
        expect(target.textContent).not.toContain(strings.interrupted)
        expect(button(strings.signIn)).toBeDefined()
    })

    it('cancels a completion still in flight when the form is disposed', async () => {
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await beginGoogleAuthorization()
        let rejectCompletion!: (reason: unknown) => void
        state.completeAuthorization.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectCompletion = reject }))
        button(strings.finishSignIn).click()
        await settle()
        await unmount(component!)
        component = undefined
        expect(state.cancelAuthorization).toHaveBeenCalledWith('authorization-1')
        rejectCompletion({ kind: 'cancelled' })
        await settle()
    })
})

describe('authorization waiting copy and renewal', () => {
    it.each([
        ['desktop', 'google_drive'], ['desktop', 'onedrive'],
        ['android', 'google_drive'], ['android', 'onedrive'],
        ['ios', 'google_drive'], ['ios', 'onedrive'],
    ] as const)('shows actionable waiting instructions on %s for %s', async (platform, provider) => {
        platformState.android = platform === 'android'
        platformState.ios = platform === 'ios'
        component = mount(ConnectionForm, { target, props: { strings, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        if (provider !== 'google_drive') await selectProvider(provider)
        await beginGoogleAuthorization()
        expect(target.textContent).toContain(platform === 'desktop' ? strings.authorizationWaiting : strings.authorizationWaitingMobile)
        expect(target.textContent?.includes(strings.manualOAuthHelp)).toBe(platform === 'android' && provider === 'google_drive')
    })

    it('renews an existing connection without editing its provider configuration', async () => {
        const connected = vi.fn()
        const renewed = { id: 'existing', providerId: 'webdav', purpose: 'backup' } as import('src/ts/storage/sync/external/types').ExternalConnectionSummary
        state.prepareRenewal.mockResolvedValue({ ...prepared, requiresOAuth: false })
        state.commitConnection.mockResolvedValue({ connection: renewed })
        component = mount(ConnectionForm, { target, props: { strings, renewalConnection: renewed, onconnected: connected, oncancel: vi.fn() } })
        await settle()
        expect(state.prepareRenewal).toHaveBeenCalledWith('existing')
        expect(target.querySelector('select')).toBeNull()
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        const password = target.querySelector<HTMLInputElement>('input[type="password"]')!
        expect(password).toBeDefined()
        typeInto(password, 'synthetic-renewed-password')
        await settle()
        button(strings.connect).click()
        await settle()
        expect(state.commitConnection).toHaveBeenCalledWith('preparation-1', { kind: 'webdav', password: 'synthetic-renewed-password' })
        expect(connected).toHaveBeenCalledWith({ connection: renewed })
        expect(state.prepareConnection).not.toHaveBeenCalled()
    })

    it.each([
        ['appDataFolder', 'Hidden app data'],
        ['', undefined],
    ])('reviews a signed-in renewal with the saved account and folder %#', async (repositoryHint, shown) => {
        const renewed = { id: 'existing', providerId: 'google_drive', purpose: 'backup' } as import('src/ts/storage/sync/external/types').ExternalConnectionSummary
        state.prepareRenewal.mockResolvedValue({ ...prepared, endpoint: { ...prepared.endpoint, accountHint: 'synthetic@example.invalid', repositoryHint } })
        component = mount(ConnectionForm, { target, props: { strings, renewalConnection: renewed, onconnected: vi.fn(), oncancel: vi.fn() } })
        await settle()
        const terms = [...target.querySelectorAll('dt')]
        expect(terms.find(term => term.textContent === strings.account)?.nextElementSibling?.textContent).toBe('synthetic@example.invalid')
        expect(terms.find(term => term.textContent === strings.repository)?.nextElementSibling?.textContent).toBe(shown)
        labelControl<HTMLInputElement>(strings.confirmEndpoint).click()
        await settle()
        expect(labelControl<HTMLInputElement>(strings.oauthClientSecret).closest('label')?.textContent).toContain(strings.renewalClientSecretHelp)
    })
})


describe('changing a saved server address', () => {
    it.each(['webdav', 's3'] as const)('names the %s connection, keeps editing open on failure and returns only the verified connection', async providerId => {
        const endpoint = { providerId, authority: 'old.example.invalid', accountHint: 'synthetic', repositoryHint: 'risunest', warnings: [], remoteVerified: true }
        const connection = { id: 'existing', providerId, purpose: 'backup', endpoint } as import('src/ts/storage/sync/external/types').ExternalConnectionSummary
        const connected = vi.fn()
        state.changeEndpoint.mockRejectedValueOnce({ kind: 'repositoryMismatch' }).mockResolvedValueOnce({ connection })
        component = mount(ConnectionForm, { target, props: { strings, addressConnection: connection, onconnected: connected, oncancel: vi.fn() } })
        await settle()
        expect(target.textContent).toContain(externalConnectionTitle(strings, connection))
        expect(target.textContent).toContain(externalConnectionPlace(strings, connection))
        typeInto(labelControl<HTMLInputElement>(strings.endpoint), 'https://new.example/dav')
        await settle()
        button(strings.saveAddress).click()
        await settle()
        expect(target.querySelector('[role="alert"]')?.textContent).toBe(strings.repositoryMismatch)
        expect(connected).not.toHaveBeenCalled()
        expect(labelControl<HTMLInputElement>(strings.endpoint).value).toBe('https://new.example/dav')
        button(strings.saveAddress).click()
        await settle()
        expect(connected).toHaveBeenCalledWith({ connection })
    })
})
