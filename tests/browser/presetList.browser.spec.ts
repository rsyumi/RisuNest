import { test, expect } from './fixture'

test('a search keeps the preset list dialog and its search field in place', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await page.goto('/presetList.html')
    const dialog = page.getByRole('dialog')
    const search = page.getByRole('searchbox', { name: 'Search presets' })
    await expect(page.locator('[data-preset-item]')).toHaveCount(8)
    const dialogBefore = (await dialog.boundingBox())!
    const searchBefore = (await search.boundingBox())!
    await search.fill('preset 7')
    await expect(page.locator('[data-preset-item]')).toHaveCount(1)
    const dialogAfter = (await dialog.boundingBox())!
    const searchAfter = (await search.boundingBox())!
    expect(Math.abs(dialogAfter.height - dialogBefore.height)).toBeLessThan(1)
    expect(Math.abs(searchAfter.y - searchBefore.y)).toBeLessThan(1)
    expect(errors).toEqual([])
})
