// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

vi.mock('src/ts/stores.svelte', async () => {
    const { createAlertQueue } = await import('src/ts/alertQueue')
    return { alertStore: createAlertQueue({ type: 'none', msg: '' }, { gapMs: 0 }) }
})

import { language } from 'src/lang'
import ChatScreenshotDialog from './ChatScreenshotDialog.svelte'
import ChatScreenshotDialogHarness from './ChatScreenshotDialogHarness.test.svelte'

const progress = (completed: number, total: number) => language.screenshotProgress
    .replace('{completed}', String(completed))
    .replace('{total}', String(total))

describe('ChatScreenshotDialog', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
    })

    test('shows total and applies Recent 50 and Full to both inclusive fields', async () => {
        mounted = mount(ChatScreenshotDialog, {
            target,
            props: { totalTurns: 120, onStart: vi.fn(), onCancel: vi.fn(), onClose: vi.fn() },
        })
        const inputs = () => [...target.querySelectorAll<HTMLInputElement>('input')]

        expect(target.textContent).toContain(language.screenshotTurns.replace('{total}', '120'))
        expect(target.textContent).toContain(language.screenshotInclusiveRange)
        expect(inputs().map((input) => input.value)).toEqual(['71', '120'])

        target.querySelector<HTMLButtonElement>('[data-full]')!.click()
        await tick()
        expect(inputs().map((input) => input.value)).toEqual(['1', '120'])

        target.querySelector<HTMLButtonElement>('[data-recent]')!.click()
        await tick()
        expect(inputs().map((input) => input.value)).toEqual(['71', '120'])
    })

    test('blocks invalid input and starts with validated numbers', async () => {
        const onStart = vi.fn()
        mounted = mount(ChatScreenshotDialog, {
            target,
            props: { totalTurns: 10, onStart, onCancel: vi.fn(), onClose: vi.fn() },
        })
        const [start, end] = [...target.querySelectorAll<HTMLInputElement>('input')]
        start.value = '8'
        start.dispatchEvent(new Event('input', { bubbles: true }))
        end.value = '3'
        end.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()

        const capture = target.querySelector<HTMLButtonElement>('[data-capture]')!
        expect(capture.disabled).toBe(true)
        expect(target.textContent).toContain(language.screenshotRangeOrder)

        end.value = '10'
        end.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        capture.click()
        expect(onStart).toHaveBeenCalledWith(8, 10)
    })

    test('shows progress and cancels a running job, including on destroy', async () => {
        const onCancel = vi.fn()
        mounted = mount(ChatScreenshotDialog, {
            target,
            props: {
                totalTurns: 10,
                running: true,
                completedTurns: 4,
                onStart: vi.fn(),
                onCancel,
                onClose: vi.fn(),
            },
        })

        expect(target.textContent).toContain(progress(4, 10))
        target.querySelector<HTMLButtonElement>('[data-cancel]')!.click()
        expect(onCancel).toHaveBeenCalledOnce()

        await unmount(mounted)
        mounted = undefined
        expect(onCancel).toHaveBeenCalledOnce()

    })

    test('cancels a running job when the dialog closes', async () => {
        const onCancel = vi.fn()
        const onClose = vi.fn()
        mounted = mount(ChatScreenshotDialog, {
            target,
            props: { totalTurns: 10, running: true, onStart: vi.fn(), onCancel, onClose },
        })

        target.querySelector<HTMLButtonElement>('button[aria-label="Cancel"]')!.click()

        expect(onCancel).toHaveBeenCalledOnce()
        expect(onClose).toHaveBeenCalledOnce()
    })

    test('closes on Escape when idle and cancels on Escape while running', async () => {
        const onCancel = vi.fn()
        const onClose = vi.fn()
        mounted = mount(ChatScreenshotDialog, {
            target,
            props: { totalTurns: 10, onStart: vi.fn(), onCancel, onClose },
        })
        await tick()

        window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
        expect(onClose).toHaveBeenCalledOnce()
        expect(onCancel).not.toHaveBeenCalled()

        await unmount(mounted)
        mounted = mount(ChatScreenshotDialog, {
            target,
            props: { totalTurns: 10, running: true, onStart: vi.fn(), onCancel, onClose },
        })
        await tick()

        window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
        expect(onCancel).toHaveBeenCalledOnce()
        expect(onClose).toHaveBeenCalledOnce()
    })

    test('keeps the turn count captured at the start of the run', async () => {
        mounted = mount(ChatScreenshotDialogHarness, {
            target,
            props: { onCancel: vi.fn(), initialRunning: false },
        })
        const harness = mounted as { setRunning(next: boolean): void, setTotalTurns(next: number): void }
        await tick()

        harness.setRunning(true)
        await tick()
        expect(target.textContent).toContain(progress(0, 10))

        harness.setTotalTurns(0)
        await tick()
        expect(target.textContent).toContain(progress(0, 10))
    })

    test('shows the error while the dialog stays open after a failed capture', async () => {
        mounted = mount(ChatScreenshotDialog, {
            target,
            props: {
                totalTurns: 10,
                running: false,
                error: 'Screenshot failed: disk full',
                onStart: vi.fn(),
                onCancel: vi.fn(),
                onClose: vi.fn(),
            },
        })

        expect(target.textContent).toContain('Screenshot failed: disk full')
        expect(target.querySelector<HTMLButtonElement>('[data-capture]')!.disabled).toBe(false)
    })

    test('cancels a running job when its parent destroys the dialog', async () => {
        const onCancel = vi.fn()
        const windowLayer = document.createElement('div')
        document.body.append(windowLayer)
        mounted = mount(ChatScreenshotDialogHarness, { target, props: { onCancel, windowLayer } })
        await tick()
        // The chat pane clips fixed layers inside it, so the dialog mounts in the chat screen's window layer.
        expect(windowLayer.querySelector('[role="dialog"]')).not.toBeNull()
        expect(target.querySelector('[role="dialog"]')).toBeNull()

        ;(mounted as { destroyDialog(): void }).destroyDialog()
        await tick()

        expect(windowLayer.querySelector('[role="dialog"]')).toBeNull()
        expect(onCancel).toHaveBeenCalledOnce()
    })
})
