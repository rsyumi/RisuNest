import type { Page } from '@playwright/test'
import { test, expect } from './fixture'

// Card and module HTML often adds fixed layers sized to the whole window. In an open chat they
// stay below the sidebar and its arrow and lay out against the chat pane, so a centered status bar
// stays whole beside the sidebar. Dialogs that cover the whole window still cover the sidebar.
const hit = (page: Page, x: number, y: number) => page.evaluate(([x, y]) => document.elementFromPoint(x, y)?.getAttribute('data-testid') ?? null, [x, y])

test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 })
})

test('fixed layers from chat HTML lay out in the chat pane and never cover the sidebar or its arrow', async ({ page }) => {
    await page.goto('/chatPane.html')
    await expect(page.getByTestId('status-bar')).toBeVisible()

    // The background fills the window behind the chat and the effect band crosses it at y 100 to 140.
    expect(await hit(page, 200, 400)).toBe('sidebar')
    expect(await hit(page, 200, 120)).toBe('sidebar')
    expect(await hit(page, 800, 120)).toBe('effect')

    const pane = await page.locator('main > div:not([data-testid])').boundingBox()
    const bar = await page.getByTestId('status-bar').boundingBox()
    expect(pane!.x).toBe(464)
    expect(bar!.x + bar!.width / 2).toBe(pane!.x + pane!.width / 2)

    // The HUD lands in the pane's top left corner, under the arrow that collapses the sidebar.
    expect(await hit(page, 480, 30)).toBe('sidebar-arrow')
    expect(await hit(page, 600, 30)).toBe('hud')
})

test('a chat dialog that covers the whole window covers the sidebar and its arrow', async ({ page }) => {
    await page.goto('/chatPane.html?dialog')
    await expect(page.getByTestId('window-dialog')).toBeAttached()

    expect(await hit(page, 200, 400)).toBe('window-dialog')
    expect(await hit(page, 480, 30)).toBe('window-dialog')
})

test('the home screen keeps its dialogs over the sidebar and its arrow', async ({ page }) => {
    await page.goto('/chatPane.html?home')
    await expect(page.getByTestId('hub-dialog')).toBeAttached()

    expect(await hit(page, 200, 400)).toBe('hub-dialog')
    expect(await hit(page, 480, 30)).toBe('hub-dialog')
})
