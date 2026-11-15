import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { invoke } from '@tauri-apps/api/core'
import { appCleanupBeforeBootstrap } from './appCleanupEntry'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
const nativeWindow = window as Window & { __TAURI_INTERNALS__?: unknown }

describe('app cleanup startup gate', () => {
    beforeEach(() => {
        vi.resetAllMocks()
        nativeWindow.__TAURI_INTERNALS__ = {}
        document.body.innerHTML = '<div id="preloading"></div>'
    })
    afterEach(() => {
        delete nativeWindow.__TAURI_INTERNALS__
        document.body.innerHTML = ''
        for (const key of ['userAgent', 'language']) Reflect.deleteProperty(navigator, key)
    })
    const stubNavigator = (values: { userAgent: string, language?: string }) => {
        for (const [key, value] of Object.entries(values)) Object.defineProperty(navigator, key, { value, configurable: true })
    }
    const windowsAgent = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36'
    const androidAgent = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36'
    const iosAgent = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148'
    // The page runs before the language module loads, so its text has no language key to compare.
    const statusText = () => document.querySelector('[role="status"]')?.textContent
    const cancelButton = () => document.querySelectorAll<HTMLButtonElement>('main button')[1]
    const cancellationNote = () => document.querySelector<HTMLElement>('main p:not([role="status"])')!
    it('skips native calls on web', async () => {
        delete nativeWindow.__TAURI_INTERNALS__
        await appCleanupBeforeBootstrap()
        expect(invoke).not.toHaveBeenCalled()
    })
    it('allows normal startup only for a nonpending status', async () => {
        vi.mocked(invoke).mockResolvedValue({ pending: false, mode: null, error: null })
        await appCleanupBeforeBootstrap()
        expect(invoke).toHaveBeenCalledExactlyOnceWith('app_cleanup_status')
        expect(document.querySelector('main')).toBeNull()
    })
    it('automatically resumes pending cleanup once and never bootstraps after resolution', async () => {
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: null }).mockResolvedValue(undefined)
        const bootstrap = vi.fn()
        void appCleanupBeforeBootstrap().then(bootstrap)
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('app_cleanup_resume'))
        expect(invoke).toHaveBeenCalledTimes(2)
        expect(bootstrap).not.toHaveBeenCalled()
        expect(document.querySelector('button')?.disabled).toBe(true)
        expect(document.getElementById('preloading')).toBeNull()
    })
    it('offers retry after failure without exposing native error strings', async () => {
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: null, canCancel: true })
            .mockRejectedValueOnce(new Error('synthetic-private-path'))
            .mockResolvedValueOnce({ pending: true, mode: 'reset', error: 'synthetic-private-path', canCancel: true })
            .mockResolvedValue(undefined)
        const bootstrap = vi.fn()
        void appCleanupBeforeBootstrap().then(bootstrap)
        await vi.waitFor(() => expect(document.querySelector('button')?.disabled).toBe(false))
        expect(document.body.textContent).not.toContain('synthetic-private-path')
        document.querySelector('button')!.click()
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledTimes(4))
        expect(bootstrap).not.toHaveBeenCalled()
    })
    it('covers the viewport with the reset page', async () => {
        // The page keeps #app at full height and does not let the body scroll.
        document.body.insertAdjacentHTML('afterbegin', '<div id="app"></div>')
        vi.mocked(invoke).mockResolvedValue({ pending: true, mode: 'reset', error: 'cleanup-files-busy-or-denied', canCancel: true })
        void appCleanupBeforeBootstrap()
        const page = await vi.waitFor(() => {
            const found = document.querySelector<HTMLElement>('main')
            expect(found).not.toBeNull()
            return found!
        })
        expect(page.style.position).toBe('fixed')
        expect(page.style.inset).toBe('0')
        expect(page.style.overflow).toBe('auto')
        expect(page.style.background).toContain('--risu-theme-bgcolor')
        expect(page.style.color).toContain('--risu-theme-textcolor')
        // The app stylesheet strips the default button look, so both buttons have to bring their own.
        for (const button of page.querySelectorAll('button')) {
            expect(button.classList).toContain('border')
            expect(button.classList).toContain('bg-darkbutton')
            expect(button.classList).toContain('disabled:opacity-50')
        }
        expect(page.querySelectorAll('button')).toHaveLength(2)
    })
    it('blocks normal startup when cleanup status is unavailable', async () => {
        vi.mocked(invoke).mockRejectedValue(new Error('status-unavailable'))
        const bootstrap = vi.fn()
        void appCleanupBeforeBootstrap().then(bootstrap)
        await vi.waitFor(() => expect(document.querySelector('button')?.disabled).toBe(false))
        expect(bootstrap).not.toHaveBeenCalled()
        expect(invoke).not.toHaveBeenCalledWith('app_cleanup_resume')
    })
    it('shows actionable WebView update guidance and still blocks bootstrap', async () => {
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: null }).mockRejectedValue('cleanup-webview-update-required')
        const bootstrap = vi.fn()
        void appCleanupBeforeBootstrap().then(bootstrap)
        await vi.waitFor(() => expect(statusText()).toContain('Android System WebView'))
        expect(document.querySelector('button')?.disabled).toBe(false)
        expect(bootstrap).not.toHaveBeenCalled()
    })
    it.each([
        ['cleanup-webview-update-required', 'Android System WebView'],
        ['cleanup-data-root-in-use', 'same data folder'],
        ['synthetic-private-path', 'Please retry'],
    ])('waits for explicit retry when a previous cleanup failed with %s', async (error, guidance) => {
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'prepare-removal', error }).mockResolvedValue(undefined)
        const bootstrap = vi.fn()
        void appCleanupBeforeBootstrap().then(bootstrap)
        await vi.waitFor(() => expect(document.querySelector('button')?.disabled).toBe(false))
        expect(statusText()).toContain(guidance)
        expect(invoke).toHaveBeenCalledExactlyOnceWith('app_cleanup_status')
        expect(document.body.textContent).not.toContain('synthetic-private-path')
        document.querySelector('button')!.click()
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('app_cleanup_resume'))
        expect(invoke).toHaveBeenCalledTimes(2)
        expect(bootstrap).not.toHaveBeenCalled()
    })

    it('allows normal startup when only cleanup is unavailable', async () => {
        vi.mocked(invoke).mockResolvedValue({ pending: false, mode: null, error: 'cleanup-path-redirected', canCancel: false })
        await appCleanupBeforeBootstrap()
        expect(document.querySelector('main')).toBeNull()
    })

    it('offers cancellation only when native status proves roots are intact', async () => {
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: 'secret-cleanup-unavailable', canCancel: true })
            .mockResolvedValue(undefined)
        const bootstrap = vi.fn()
        void appCleanupBeforeBootstrap().then(bootstrap)
        await vi.waitFor(() => expect(statusText()).toContain('Linux'))
        expect(cancellationNote().hidden).toBe(false)
        expect(cancelButton().hidden).toBe(false)
        cancelButton().click()
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('app_cleanup_cancel'))
        expect(bootstrap).not.toHaveBeenCalled()
    })

    it('refreshes cancellation eligibility after a retry enters root deletion', async () => {
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: 'synthetic-unknown-failure', canCancel: false })
        void appCleanupBeforeBootstrap()
        await vi.waitFor(() => expect(statusText()).toBeTruthy())
        const unknownFailure = statusText()
        document.body.innerHTML = ''
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: 'secret-cleanup-unavailable', canCancel: true })
            .mockRejectedValueOnce('cleanup-files-busy-or-denied')
            .mockResolvedValueOnce({ pending: true, mode: 'reset', error: 'cleanup-files-busy-or-denied', canCancel: false })
        void appCleanupBeforeBootstrap()
        await vi.waitFor(() => expect(document.querySelector('button')?.disabled).toBe(false))
        const credentialStoreFailure = statusText()
        expect(cancelButton().hidden).toBe(false)
        document.querySelector('button')!.click()
        await vi.waitFor(() => expect(cancelButton().hidden).toBe(true))
        // The file access failure has its own guidance, distinct from the earlier and the fallback message.
        expect(statusText()).not.toBe(credentialStoreFailure)
        expect(statusText()).not.toBe(unknownFailure)
        expect(cancellationNote().hidden).toBe(true)
        expect(invoke).not.toHaveBeenCalledWith('app_cleanup_cancel')
    })

    it.each([
        [windowsAgent, null, 'C:/synthetic/agent-cleanup'],
        [windowsAgent, 'work', 'D:/synthetic/work/cleanup'],
        ['Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)', 'work', '/synthetic/mac/work/cleanup'],
        ['Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko)', 'work', '/custom-xdg/risunest-profiles/work/cleanup'],
    ])('keeps a corrupt journal fail closed and names the record to remove outside the app (%#)', async (userAgent, profile, folder) => {
        stubNavigator({ userAgent, language: 'en-US' })
        vi.mocked(invoke).mockResolvedValue({ pending: true, mode: null, error: 'cleanup-journal-corrupt', canCancel: false, profile, namedProfiles: profile !== null, cleanupFolder: folder })
        const bootstrap = vi.fn()
        void appCleanupBeforeBootstrap().then(bootstrap)
        await vi.waitFor(() => expect(statusText()).toContain(folder))
        expect(cancelButton().hidden).toBe(true)
        expect(bootstrap).not.toHaveBeenCalled()
    })

    it('tells Android users to clear the app data when the record is damaged', async () => {
        const instruction = async (userAgent: string) => {
            document.body.innerHTML = ''
            stubNavigator({ userAgent, language: 'ko-KR' })
            void appCleanupBeforeBootstrap()
            await vi.waitFor(() => expect(statusText()).toBeTruthy())
            expect(cancelButton().hidden).toBe(true)
            return statusText()!
        }
        vi.mocked(invoke).mockResolvedValue({ pending: true, mode: null, error: 'cleanup-journal-corrupt', canCancel: false })
        const android = await instruction(androidAgent)
        expect(android).toMatch(/[가-힣]/)
        expect(android).not.toMatch(/RisuNest-cleanup|risunest-cleanup/)
        expect(android).not.toBe(await instruction(iosAgent))
    })

    it('points to Cancel reset when a damaged credential record left the data intact', async () => {
        stubNavigator({ userAgent: windowsAgent, language: 'ko-KR' })
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: 'secret-index-corrupt', canCancel: true })
            .mockResolvedValue(undefined)
        void appCleanupBeforeBootstrap()
        await vi.waitFor(() => expect(statusText()).toMatch(/[가-힣]/))
        expect(statusText()).toContain(cancelButton().textContent)
        expect(statusText()).not.toContain('RisuNest-cleanup')
        expect(cancelButton().hidden).toBe(false)
        cancelButton().click()
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('app_cleanup_cancel'))
    })

    it('falls back to removing the record outside the app when cancellation is unavailable', async () => {
        stubNavigator({ userAgent: windowsAgent, language: 'en-US' })
        vi.mocked(invoke).mockResolvedValue({ pending: true, mode: 'reset', error: 'secret-index-corrupt', canCancel: false, cleanupFolder: 'C:/synthetic/agent-cleanup' })
        void appCleanupBeforeBootstrap()
        await vi.waitFor(() => expect(statusText()).toContain('C:/synthetic/agent-cleanup'))
        expect(statusText()).not.toContain(cancelButton().textContent)
        expect(cancelButton().hidden).toBe(true)
    })

    it('reloads safely if cancellation cleared the journal but navigation failed', async () => {
        const reload = vi.spyOn(location, 'reload').mockImplementation(() => {})
        vi.mocked(invoke).mockResolvedValueOnce({ pending: true, mode: 'reset', error: 'secret-cleanup-unavailable', canCancel: true })
            .mockRejectedValueOnce('cleanup-navigation-failed')
            .mockResolvedValueOnce({ pending: false, mode: null, error: null, canCancel: false })
        void appCleanupBeforeBootstrap()
        await vi.waitFor(() => expect(document.querySelectorAll('button')).toHaveLength(2))
        document.querySelectorAll('button')[1].click()
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledTimes(3))
        document.querySelector('button')!.click()
        await vi.waitFor(() => expect(reload).toHaveBeenCalledOnce())
        expect(invoke).not.toHaveBeenCalledWith('app_cleanup_resume')
        reload.mockRestore()
    })
})
