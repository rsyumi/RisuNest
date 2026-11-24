import { test, expect } from './fixture'

// Card and module HTML often adds fixed layers sized to the whole window. In the chat pane they
// stay below the sidebar and lay out against the pane, so a centered status bar stays whole.
test('fixed layers from chat HTML lay out in the chat pane and never cover the sidebar', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.goto('/chatPane.html')
    await expect(page.getByTestId('status-bar')).toBeVisible()

    const hit = (x: number, y: number) => page.evaluate(([x, y]) => document.elementFromPoint(x, y)?.getAttribute('data-testid') ?? null, [x, y])
    // The background fills the window behind the chat and the effect band crosses it at y 100 to 140.
    expect(await hit(200, 400)).toBe('sidebar')
    expect(await hit(200, 120)).toBe('sidebar')
    expect(await hit(800, 120)).toBe('effect')

    const pane = await page.locator('main > div:not([data-testid])').boundingBox()
    const bar = await page.getByTestId('status-bar').boundingBox()
    expect(pane!.x).toBe(464)
    expect(bar!.x + bar!.width / 2).toBe(pane!.x + pane!.width / 2)
})
