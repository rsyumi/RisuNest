import { test, expect } from './fixture'
import { languageKorean as language } from '../../src/lang/ko'

declare global { interface Window { appLockNative: { holdReady(): void; pendingReady(): boolean; finishReady(focusFirst: boolean): void; available(): void; presentations(): boolean[]; authScreens(): boolean[] }; lazyApp: { open(route: string): void; layout(mobile: boolean): void; read(): { sidebarClosing: boolean; entries: number }; lock: { initialize(nativeUnlock?: boolean): Promise<void>; bootFailure(loaded: boolean): void; requestExit(): void; changePin(): void; loading(value: boolean): void; loadingAlert(): void }; plugin: { load(script: string): Promise<void>; writes: { key: string; value: unknown }[] } }; lazyImportBoundary: { arm(route: string): void; hold(route: string): void; rejectHeld(): void; attempts(): number } } }

const hosts = ['settings', 'custom', 'grid', 'presets', 'personas', 'mobile'] as const
for (const back of ['header', 'Escape', 'history'] as const) {
    test(`actual mobile chat menu opens Settings and returns through ${back}`, async ({ page }) => {
        const errors: string[] = []
        page.on('pageerror', error => errors.push(error.message))
        await page.setViewportSize({ width: 390, height: 844 })
        await page.goto('/lazyApp.html')
        await page.evaluate(() => window.lazyApp.open('mobile-chat'))
        await page.getByRole('button', { name: language.menu, exact: true }).click()
        await page.getByRole('button', { name: language.settings, exact: true }).click()
        const display = page.getByRole('button', { name: language.display, exact: true })
        await display.click()
        await expect(display).toHaveCount(0)
        const goBack = async () => {
            if (back === 'header') await page.getByRole('button', { name: language.goback, exact: true }).click()
            else if (back === 'Escape') await page.keyboard.press('Escape')
            else await page.evaluate(() => history.back())
        }
        await goBack()
        await expect(display).toBeVisible()
        await goBack()
        await expect(display).toHaveCount(0)
        await expect(page.getByRole('button', { name: language.settings, exact: true })).toBeVisible()
        await page.getByRole('button', { name: language.goback, exact: true }).click()
        await expect(page.getByRole('button', { name: language.menu, exact: true })).toBeVisible()
        expect(errors).toEqual([])
    })
}
test('actual Settings returns to the selected mobile menu after layout changes', async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 })
    await page.goto('/lazyApp.html')
    await page.evaluate(() => window.lazyApp.open('mobile-chat'))
    await page.getByRole('button', { name: language.menu, exact: true }).click()
    await page.getByRole('button', { name: language.settings, exact: true }).click()
    const display = page.getByRole('button', { name: language.display, exact: true })
    await expect(display).toBeVisible()
    await page.setViewportSize({ width: 1200, height: 800 })
    await page.evaluate(() => window.lazyApp.layout(false))
    await expect(display).toBeVisible()
    await page.setViewportSize({ width: 390, height: 844 })
    await page.evaluate(() => window.lazyApp.layout(true))
    // The page the desktop layout opened stays open, so the menu list is hidden until Back.
    await expect(display).toHaveCount(0)
    await page.getByRole('button', { name: language.goback, exact: true }).click()
    await expect(display).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(display).toHaveCount(0)
    await expect(page.getByRole('button', { name: language.settings, exact: true })).toBeVisible()
})

for (const host of hosts) {
    for (const action of ['retry', 'leave'] as const) {
        test(`actual App ${host} rejected import supports ${action}`, async ({ page }) => {
            const errors: string[] = []
            page.on('pageerror', error => errors.push(error.message))
            await page.goto('/lazyApp.html')
            await page.evaluate(route => window.lazyImportBoundary.arm(route), host)
            const open = async () => host === 'grid'
                ? page.getByRole('button', { name: 'Synthetic grid opener', exact: true }).click()
                : page.evaluate(route => window.lazyApp.open(route), host)
            await open()
            const failure = page.getByRole('alert')
            await expect(failure.getByText(host === 'settings' || host === 'mobile' ? language.risuNest.lazy.settings : language.risuNest.lazy.screen, { exact: true })).toBeVisible()
            const backLabel = host === 'mobile' ? language.goback : language.close
            await expect(failure.getByRole('button')).toHaveText([language.retry, backLabel])
            if (action === 'retry') await failure.getByRole('button', { name: language.retry, exact: true }).click()
            else {
                await failure.getByRole('button', { name: backLabel, exact: true }).click()
                await expect(failure).toHaveCount(0)
                await open()
            }
            await expect(page.locator('[data-lazy-loaded]')).toHaveCount(1)
            await expect(failure).toHaveCount(0)
            expect(await page.evaluate(() => window.lazyImportBoundary.attempts())).toBe(2)
            expect(errors).toEqual([])
        })
    }
}

test('actual MobileFooter leaving a failed Settings import allows re-entry', async ({ page }) => {
    await page.goto('/lazyApp.html')
    await page.evaluate(() => window.lazyImportBoundary.arm('mobile'))
    await page.evaluate(() => window.lazyApp.open('mobile'))
    await expect(page.getByRole('alert')).toBeVisible()
    await page.getByRole('button', { name: language.character, exact: true }).click()
    await expect(page.getByRole('alert')).toHaveCount(0)
    await page.getByRole('button', { name: language.settings, exact: true }).click()
    await expect(page.locator('[data-lazy-loaded]')).toHaveCount(1)
    expect(await page.evaluate(() => window.lazyImportBoundary.attempts())).toBe(2)
})

test('actual MobileBody ignores a prior import rejection after footer leave and re-entry', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await page.goto('/lazyApp.html')
    await page.evaluate(() => { window.lazyImportBoundary.hold('mobile'); window.lazyApp.open('mobile') })
    await expect(page.getByRole('status')).toBeVisible()
    await page.getByRole('button', { name: language.character, exact: true }).click()
    await expect(page.getByRole('status')).toHaveCount(0)
    await page.getByRole('button', { name: language.settings, exact: true }).click()
    await expect(page.locator('[data-lazy-loaded]')).toHaveCount(1)
    expect(await page.evaluate(() => window.lazyImportBoundary.attempts())).toBe(2)
    await page.evaluate(() => window.lazyImportBoundary.rejectHeld())
    await expect(page.locator('[data-lazy-loaded]')).toHaveCount(1)
    await expect(page.getByRole('alert')).toHaveCount(0)
    expect(errors).toEqual([])
})

test('actual App closes the open sidebar on Back and leaves Escape to its contents', async ({ page }) => {
    await page.goto('/lazyApp.html')
    await page.evaluate(() => window.lazyApp.open('sidebar'))
    await expect(page.getByRole('button', { name: 'Synthetic grid opener', exact: true })).toBeVisible()
    await expect.poll(() => page.evaluate(() => window.lazyApp.read().entries)).toBe(1)
    await page.keyboard.press('Escape')
    expect(await page.evaluate(() => window.lazyApp.read())).toEqual({ sidebarClosing: false, entries: 1 })
    await page.evaluate(() => history.back())
    await expect.poll(() => page.evaluate(() => window.lazyApp.read().sidebarClosing)).toBe(true)
    expect(page.url()).toMatch(/\/lazyApp\.html$/)
})


test.describe('actual App lock integration', () => {
    test.beforeEach(async ({ page }) => {
        await page.addInitScript(() => {
            let callback = 0
            let holdReady = false
            let available = false
            const presentations: boolean[] = []
            const authScreens: boolean[] = []
            let finishReady: ((focusFirst: boolean) => void) | undefined
            Object.assign(window, {
                appLockNative: {
                    holdReady: () => { holdReady = true },
                    available: () => { available = true },
                    presentations: () => presentations,
                    authScreens: () => authScreens,
                    pendingReady: () => !!finishReady,
                    finishReady: (focusFirst: boolean) => { finishReady!(focusFirst); finishReady = undefined; holdReady = false },
                },
                __TAURI_INTERNALS__: {
                    metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
                    transformCallback: () => ++callback,
                    invoke: async (command: string, args?: { locked: boolean }) => {
                        if (command === 'app_lock_check') return { auth: available ? { status: 'available', method: 'windowsHello' } : { status: 'unsupported' }, previewSupported: false }
                        if (command === 'app_lock_authenticate') {
                            authScreens.push(!!document.querySelector('.app-lock-screen') && !!document.querySelector('main')?.inert)
                            return 'cancelled'
                        }
                        if (command === 'app_lock_set_policy') return { active: true, epoch: 0, atMs: 0, departedAtMs: null }
                        if (command === 'app_lock_ready') {
                            presentations.push(args!.locked)
                            if (!holdReady) return true
                            return new Promise(resolve => {
                                finishReady = focusFirst => {
                                    if (focusFirst) document.querySelector<HTMLElement>('.app-lock-screen button')?.focus()
                                    resolve(true)
                                }
                            })
                        }
                        if (command === 'plugin:event|listen') return 1
                        if (command === 'diagnostic_device_info') return { model: 'Synthetic', arch: 'synthetic' }
                        throw new Error(`Unexpected synthetic native command: ${command}`)
                    },
                },
            })
        })
        await page.goto('/lazyApp.html')
    })

    const painted = (page: import('@playwright/test').Page) => page.evaluate(() => new Promise<void>(resolve => {
        requestAnimationFrame(() => requestAnimationFrame(() => resolve()))
    }))

    test('presents loading and its confirmation before markers, then holds a startup confirmation behind the PIN screen', async ({ page }) => {
        await page.goto('/lazyApp.html?loading')
        const loading = page.getByRole('status').filter({ hasText: language.loading })
        await expect(loading).toBeVisible()
        await expect.poll(() => page.evaluate(() => window.appLockNative.presentations().length)).toBeGreaterThan(0)
        await page.evaluate(() => { window.appLockNative.available(); window.lazyApp.lock.loadingAlert() })
        await expect(page.getByText('Synthetic startup confirmation')).toBeVisible()
        await page.getByRole('button', { name: 'YES', exact: true }).click()
        await expect(page.getByText('Synthetic startup confirmation')).toHaveCount(0)
        const before = await page.evaluate(() => window.appLockNative.presentations().length)
        await page.evaluate(() => window.lazyApp.lock.initialize(true))
        await expect(loading).toBeVisible()
        await expect(page.locator('main')).not.toHaveAttribute('inert')
        await expect.poll(() => page.evaluate(() => window.appLockNative.presentations().length)).toBeGreaterThan(before)
        expect(await page.evaluate(() => window.appLockNative.presentations().at(-1))).toBe(true)
        expect(await page.evaluate(() => window.appLockNative.authScreens())).toEqual([])
        await page.evaluate(() => window.lazyApp.plugin.load(`
            document.body.textContent = 'Synthetic plugin during startup';
            await risuai.showContainer('fullscreen');
            await risuai.pluginStorage.setItem('startup-shown', true);
        `))
        await expect.poll(() => page.evaluate(() => window.lazyApp.plugin.writes)).toContainEqual({ key: 'startup-shown', value: true })
        const startupPlugin = page.locator('iframe[data-risu-plugin-frame]')
        await expect(startupPlugin).toBeHidden()
        expect(await startupPlugin.ariaSnapshot()).toBe('')
        await page.evaluate(() => window.lazyApp.lock.loadingAlert())
        const gate = page.getByRole('dialog', { name: 'RisuNest' })
        await expect(gate).toBeVisible()
        await expect(page.locator('main')).toHaveAttribute('inert', '')
        await expect.poll(() => page.evaluate(() => window.appLockNative.authScreens())).toEqual([true])
        await page.keyboard.type('123456')
        await expect(gate).toHaveCount(0)
        await expect(page.locator('main')).not.toHaveAttribute('inert')
        await expect(loading).toBeVisible()
        await expect.poll(() => page.evaluate(() => window.appLockNative.presentations().at(-1))).toBe(false)
        await page.getByRole('button', { name: 'YES', exact: true }).click()
        await expect(page.getByText('Synthetic startup confirmation')).toHaveCount(0)
        await page.evaluate(() => window.lazyApp.lock.loading(false))
        await expect(page.locator('main')).not.toHaveAttribute('inert')
        await expect(gate).toHaveCount(0)
    })

    test('restores initial dialog focus after the native cover automatically focuses the first PIN button', async ({ page }) => {
        await page.evaluate(() => window.appLockNative.holdReady())
        await page.evaluate(() => window.lazyApp.lock.initialize())
        const gate = page.getByRole('dialog', { name: 'RisuNest' })
        await expect.poll(() => page.evaluate(() => window.appLockNative.pendingReady())).toBe(true)
        await page.evaluate(() => window.appLockNative.finishReady(true))
        await expect(gate).toBeFocused()
        const presentations = await page.evaluate(() => window.appLockNative.presentations().length)
        await page.keyboard.press('Tab')
        await expect(gate.getByRole('button', { name: '1', exact: true })).toBeFocused()
        await page.keyboard.press('Enter')
        await expect(gate.getByRole('img', { name: language.risuNest.appLock.progress.replace('{0}', '1') })).toBeVisible()
        await painted(page)
        await painted(page)
        await expect(gate.getByRole('button', { name: '1', exact: true })).toBeFocused()
        await page.keyboard.press('Space')
        await expect(gate.getByRole('img', { name: language.risuNest.appLock.progress.replace('{0}', '2') })).toBeVisible()
        await painted(page)
        await painted(page)
        await expect(gate.getByRole('button', { name: '1', exact: true })).toBeFocused()
        expect(await page.evaluate(() => window.appLockNative.presentations().length)).toBe(presentations)
    })

    test('preserves user keyboard focus when native presentation finishes late', async ({ page }) => {
        await page.evaluate(() => window.appLockNative.holdReady())
        await page.evaluate(() => window.lazyApp.lock.initialize())
        const gate = page.getByRole('dialog', { name: 'RisuNest' })
        await expect.poll(() => page.evaluate(() => window.appLockNative.pendingReady())).toBe(true)
        await page.keyboard.press('Tab')
        const digit = gate.getByRole('button', { name: '1', exact: true })
        await expect(digit).toBeFocused()
        await page.evaluate(() => window.appLockNative.finishReady(false))
        await painted(page)
        await expect(digit).toBeFocused()
        await page.keyboard.press('Enter')
        await expect(gate.getByRole('img', { name: language.risuNest.appLock.progress.replace('{0}', '1') })).toBeVisible()
    })

    test('preserves user pointer focus when native presentation finishes late', async ({ page }) => {
        await page.evaluate(() => window.appLockNative.holdReady())
        await page.evaluate(() => window.lazyApp.lock.initialize())
        const gate = page.getByRole('dialog', { name: 'RisuNest' })
        await expect.poll(() => page.evaluate(() => window.appLockNative.pendingReady())).toBe(true)
        const digit = gate.getByRole('button', { name: '2', exact: true })
        await digit.dispatchEvent('pointerdown')
        await digit.focus()
        await page.evaluate(() => window.appLockNative.finishReady(false))
        await painted(page)
        await expect(digit).toBeFocused()
    })

    test('shakes mismatched confirmation, restarts PIN entry, and respects reduced motion', async ({ page }) => {
        await page.evaluate(() => window.lazyApp.lock.initialize())
        await page.keyboard.type('123456')
        await expect(page.getByRole('dialog', { name: 'RisuNest' })).toHaveCount(0)
        await page.evaluate(() => window.lazyApp.lock.changePin())
        const gate = page.getByRole('dialog', { name: language.risuNest.appLock.changeTitle })
        await page.keyboard.type('123456654321')
        await expect(gate.getByRole('alert')).toHaveText(language.risuNest.appLock.mismatch)
        await expect(gate.getByText(language.risuNest.appLock.newPin, { exact: true })).toBeVisible()
        const dots = gate.getByRole('img', { name: language.risuNest.appLock.progress.replace('{0}', '0') })
        await expect(dots).toBeVisible()
        expect(await dots.evaluate(node => getComputedStyle(node).animationName)).not.toBe('none')
        await expect(dots).toHaveCSS('animation-duration', '0.36s')
        await page.emulateMedia({ reducedMotion: 'reduce' })
        await expect(dots).toHaveCSS('animation-name', 'none')
        await page.emulateMedia({ reducedMotion: 'no-preference' })
        await page.keyboard.type('4')
        await expect(gate.getByRole('alert')).toBeEmpty()
        await expect(gate.getByRole('img', { name: language.risuNest.appLock.progress.replace('{0}', '1') })).toHaveCSS('animation-name', 'none')
    })

    test('retains the mounted draft and restores focus after PIN unlock despite a post-load boot failure', async ({ page }) => {
        const errors: string[] = []
        page.on('pageerror', error => errors.push(error.message))
        const draft = page.getByRole('textbox', { name: 'Synthetic retained draft' })
        await draft.fill('Retained draft')
        await draft.evaluate(node => { node.dataset.retained = 'original' })
        await page.evaluate(() => window.appLockNative.holdReady())
        await page.evaluate(() => window.lazyApp.lock.initialize())
        await expect.poll(() => page.evaluate(() => window.appLockNative.pendingReady())).toBe(true)
        const gate = page.getByRole('dialog', { name: 'RisuNest' })
        await expect(gate).toBeVisible()
        await expect(page.locator('main')).toHaveAttribute('inert', '')
        await page.evaluate(() => window.lazyApp.lock.bootFailure(true))
        await expect(gate).toBeVisible()
        await expect(page.getByRole('heading', { name: language.risuNest.boot.title })).toHaveCount(0)
        await expect(page.locator('main')).toHaveAttribute('inert', '')
        await page.keyboard.type('123456')
        await expect(gate).toHaveCount(0)
        await page.evaluate(() => window.appLockNative.finishReady(false))
        await painted(page)
        await expect(draft).toBeFocused()
        await expect(draft).toHaveValue('Retained draft')
        await expect(draft).toHaveAttribute('data-retained', 'original')
        await page.keyboard.type(' preserved')
        await expect(draft).toHaveValue('Retained draft preserved')
        expect(errors).toEqual([])
    })

    test('isolates fullscreen plugin input during locking and resumes its retained UI after unlock', async ({ page }) => {
        await page.evaluate(() => window.lazyApp.plugin.load(`
            const input = document.createElement('input');
            input.setAttribute('aria-label', 'Synthetic plugin draft');
            document.body.append(input);
            window.addEventListener('message', async event => {
                if (event.data === 'focus-input') input.focus();
                if (event.data === 'background-work') {
                    await risuai.hideContainer();
                    await risuai.showContainer('fullscreen');
                    await risuai.pluginStorage.setItem('background-work', 'saved');
                }
            });
            await risuai.showContainer('fullscreen');
            await risuai.pluginStorage.setItem('shown', true);
        `))
        await expect.poll(() => page.evaluate(() => window.lazyApp.plugin.writes)).toContainEqual({ key: 'shown', value: true })
        const frame = page.locator('iframe[data-risu-plugin-frame]')
        const draft = frame.contentFrame().getByRole('textbox', { name: 'Synthetic plugin draft' })
        await draft.fill('Retained plugin draft')
        await frame.evaluate(node => { node.dataset.retained = 'original' })
        await page.evaluate(() => window.lazyApp.lock.initialize())
        const gate = page.getByRole('dialog', { name: 'RisuNest' })
        await expect(gate).toBeVisible()
        await expect(frame).toBeHidden()
        expect(await frame.ariaSnapshot()).toBe('')
        await frame.evaluate(node => (node as HTMLIFrameElement).contentWindow!.postMessage('focus-input', '*'))
        await painted(page)
        await page.keyboard.type('4')
        await expect(gate.getByRole('img', { name: language.risuNest.appLock.progress.replace('{0}', '1') })).toBeVisible()
        await expect(frame.contentFrame().locator('input')).toHaveValue('Retained plugin draft')
        await frame.evaluate(node => (node as HTMLIFrameElement).contentWindow!.postMessage('background-work', '*'))
        await expect.poll(() => page.evaluate(() => window.lazyApp.plugin.writes)).toContainEqual({ key: 'background-work', value: 'saved' })
        await expect(frame).toBeHidden()
        await page.keyboard.press('Backspace')
        await page.keyboard.type('123456')
        await expect(gate).toHaveCount(0)
        await expect(frame).toHaveAttribute('data-retained', 'original')
        await draft.fill('Retained plugin draft resumed')
        await expect(draft).toHaveValue('Retained plugin draft resumed')
    })

    test('shows the startup failure panel without a PIN gate when normal content never loaded', async ({ page }) => {
        await page.evaluate(() => window.lazyApp.lock.initialize())
        await page.evaluate(() => window.lazyApp.lock.bootFailure(false))
        await expect(page.getByRole('heading', { name: language.risuNest.boot.title })).toBeVisible()
        await expect(page.getByRole('dialog', { name: 'RisuNest' })).toHaveCount(0)
        await expect(page.locator('main')).not.toHaveAttribute('inert')
    })

    test('tabs through visible exit actions and returns focus to the locked screen on cancel', async ({ page }) => {
        await page.evaluate(() => window.appLockNative.holdReady())
        await page.evaluate(() => window.lazyApp.lock.initialize())
        await expect.poll(() => page.evaluate(() => window.appLockNative.pendingReady())).toBe(true)
        const gate = page.getByRole('dialog', { name: 'RisuNest' })
        await expect(gate).toBeVisible()
        await page.evaluate(() => window.lazyApp.lock.requestExit())
        const exit = page.getByTestId('sync-exit-dialog')
        await expect(exit).toBeFocused()
        await page.evaluate(() => window.appLockNative.finishReady(false))
        await painted(page)
        await expect(exit).toBeFocused()
        const summary = exit.locator('summary')
        await page.keyboard.press('Tab')
        await expect(summary).toBeFocused()
        await page.keyboard.press('Tab')
        await expect(exit.getByRole('button', { name: language.risuNest.exitDrain.cancelExit, exact: true })).toBeFocused()
        await page.keyboard.press('Shift+Tab')
        await page.keyboard.press('Enter')
        await expect(exit.locator('details')).toHaveAttribute('open', '')
        await page.keyboard.press('Tab')
        await expect(exit.getByRole('button', { name: language.copy, exact: true })).toBeFocused()
        await page.keyboard.press('Escape')
        await expect(exit).toHaveCount(0)
        await expect(gate).toBeFocused()
        await expect(page.locator('main')).toHaveAttribute('inert', '')
        await page.keyboard.type('123456')
        await expect(gate).toHaveCount(0)
    })
})
