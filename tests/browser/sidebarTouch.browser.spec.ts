import { test, expect } from './fixture'
import type { Locator, Page } from '@playwright/test'

test.use({ hasTouch: true, isMobile: true, viewport: { width: 393, height: 852 } })

type Point = { x: number, y: number }

async function centre(target: Locator): Promise<Point> {
    const box = (await target.boundingBox())!
    return { x: box.x + box.width / 2, y: box.y + box.height / 2 }
}

/** Puts a finger on the source and keeps it down until release. */
async function press(page: Page, hold: number, source: Locator | Point) {
    let at = 'boundingBox' in source ? await centre(source) : source
    const cdp = await page.context().newCDPSession(page)
    await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [at] })
    await page.waitForTimeout(hold)
    return {
        async to(target: Locator | Point) {
            const from = at
            const end = 'boundingBox' in target ? await centre(target) : target
            for (let step = 1; step <= 3; step++) {
                at = { x: from.x + (end.x - from.x) * step / 3, y: from.y + (end.y - from.y) * step / 3 }
                await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [at] })
                await page.waitForTimeout(35)
            }
        },
        async release() {
            await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] })
            await cdp.detach()
        },
    }
}

async function move(page: Page, hold: number, source: string | Locator, target: string | Locator) {
    const locate = (spot: string | Locator) => typeof spot === 'string' ? page.getByRole('button', { name: spot, exact: true }) : spot
    const from = await centre(locate(source))
    const to = await centre(locate(target))
    const finger = await press(page, hold, from)
    await finger.to({ x: from.x, y: to.y })
    await finger.release()
}

test('tap, early swipe and held drag coexist with the iOS polyfill', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await page.goto('/sidebarTouch.html')
    await page.getByRole('button', { name: 'Bot 1', exact: true }).tap()
    await expect(page.getByText('sidebar-clicks:1', { exact: true })).toBeVisible()
    await page.getByRole('button', { name: 'Reset sidebar' }).tap()
    // A tap during inertial scrolling stops the fling instead of activating Reset.
    const scrollEnded = page.locator('#touch-sidebar').evaluate(list => new Promise<void>(resolve => {
        list.addEventListener('scrollend', () => resolve(), { once: true })
    }))
    await move(page, 50, 'Bot 4', 'Bot 1')
    await expect(page.getByText('sidebar-scrolled', { exact: true })).toBeVisible()
    await expect(page.getByText('sidebar-drops:0', { exact: true })).toBeVisible()
    await expect(page.getByText('sidebar-clicks:0', { exact: true })).toBeVisible()
    await scrollEnded
    await page.getByRole('button', { name: 'Reset sidebar' }).tap()
    await expect(page.getByText('sidebar-top', { exact: true })).toBeVisible()
    await move(page, 600, 'Bot 3', 'Before Bot 1')
    await expect(page.getByText('sidebar-first:3', { exact: true })).toBeVisible()
    await expect(page.getByText('sidebar-drops:1', { exact: true })).toBeVisible()
    await expect(page.getByText('sidebar-clicks:0', { exact: true })).toBeVisible()
    await expect(page.locator('.risu-drag-card')).toHaveCount(0)
    expect(errors).toEqual([])
})

test('real sidebar reorders bots and preserves folder creation, entry and exit', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await page.goto('/sidebarTouch.html?product')
    const bot = (id: number) => page.locator(`[data-sidebar-drag-id="bot-${id}"]`)
    const order = () => page.evaluate(() => (window as any).sidebarModel.db.characterOrder)
    const priority = () => page.evaluate(() => (window as any).sidebarImagePriority())
    // Rail rows whose avatar shows a drop mark. The marks follow the theme, so the test sets its own border colour.
    const marked = () => page.locator('.rs-sidebar [data-sidebar-drag-id]').evaluateAll(rows => rows
        .filter(row => Array.from(row.querySelectorAll('.sidebar-avatar')).some(avatar => getComputedStyle(avatar).outlineStyle !== 'none'))
        .map(row => (row as HTMLElement).dataset.sidebarDragId))
    const theme = 'rgb(1, 2, 3)'
    const firstGap = page.locator('[data-sidebar-drop]').first()
    await page.evaluate(colour => document.documentElement.style.setProperty('--risu-theme-borderc', colour), theme)
    await expect(bot(3)).toBeVisible()
    await expect.poll(async () => (await priority())?.visible.slice(0, 3)).toEqual(['assets/bot-1.png', 'assets/bot-2.png', 'assets/bot-3.png'])
    expect((await priority()).ordered).toEqual([])
    await move(page, 600, bot(3), firstGap)
    await expect.poll(async () => (await order()).slice(0, 3)).toEqual(['bot-3', 'bot-1', 'bot-2'])
    // A held bot marks the bot it would make a folder with, never itself, and one mark at a time with the gaps.
    let finger = await press(page, 600, bot(3))
    // Neither the held bot nor, once it moves, the dragged one gets a box around its row; the dragged one stays behind dimmed.
    await expect(bot(3)).toHaveCSS('outline-style', 'none')
    await finger.to(bot(1))
    await expect.poll(marked).toEqual(['bot-1'])
    await expect(bot(1).locator('.sidebar-avatar')).toHaveCSS('outline-color', theme)
    await expect(bot(3)).toHaveCSS('outline-style', 'none')
    await expect(bot(3)).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
    expect(await bot(3).locator('.sidebar-avatar').evaluate(avatar => {
        let opacity = 1
        for (let element: Element | null = avatar; element; element = element.parentElement) opacity *= Number(getComputedStyle(element).opacity)
        return opacity
    })).toBeCloseTo(0.45, 2)
    await finger.to(bot(3))
    await expect.poll(marked).toEqual([])
    const gap = bot(1).locator('xpath=following-sibling::*[1]')
    await finger.to(gap)
    await expect(gap).toHaveCSS('background-color', theme)
    // The card under the finger keeps no box of its own and lets the gap show through.
    expect(await page.locator('.risu-drag-card').evaluate(card => {
        const style = getComputedStyle(card)
        return [Number(style.opacity) < 1, style.outlineStyle, style.backgroundColor]
    })).toEqual([true, 'none', 'rgba(0, 0, 0, 0)'])
    await finger.to(bot(1))
    await expect.poll(marked).toEqual(['bot-1'])
    await expect(gap).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
    await finger.to(page.locator('.setting-area'))
    await expect.poll(marked).toEqual([])
    await finger.to(bot(1))
    await expect.poll(marked).toEqual(['bot-1'])
    await finger.release()
    await expect.poll(async () => (await order())[0].data).toEqual(['bot-3', 'bot-1'])
    await expect.poll(marked).toEqual([])
    await page.evaluate(() => { (window as any).sidebarModel.db.characterOrder[0].imgFile = 'assets/folder.png' })
    await expect.poll(async () => (await priority()).ordered.slice(0, 2)).toEqual(['assets/folder.png', 'assets/bot-2.png'])
    expect((await priority()).ordered).not.toContain('assets/bot-3.png')
    const folder = page.locator('[data-sidebar-drag-id^="folder:"]')
    finger = await press(page, 600, bot(2))
    await finger.to(folder)
    await expect.poll(marked).toEqual([await folder.getAttribute('data-sidebar-drag-id')])
    await finger.release()
    await expect.poll(async () => (await order())[0].data).toEqual(['bot-3', 'bot-1', 'bot-2'])
    await folder.locator('.avatar').tap()
    await expect(bot(2)).toBeVisible()
    await expect.poll(async () => (await priority()).ordered.slice(0, 4)).toEqual(['assets/folder.png', 'assets/bot-3.png', 'assets/bot-1.png', 'assets/bot-2.png'])
    // A drop on a bot inside a folder does nothing, so nothing is marked there; the gaps of the folder are.
    const folderGap = bot(3).locator('xpath=preceding-sibling::*[1]')
    finger = await press(page, 600, bot(2))
    await finger.to(bot(1))
    expect(await marked()).toEqual([])
    await finger.to(gap)
    await expect(gap).toHaveCSS('background-color', theme)
    await finger.to(folderGap)
    await expect(folderGap).toHaveCSS('background-color', theme)
    await finger.to(firstGap)
    await finger.release()
    await expect.poll(async () => (await order())[0]).toBe('bot-2')
    await expect.poll(async () => (await order())[1].data).toEqual(['bot-3', 'bot-1'])
    // A folder dropped on a bot or into a folder does nothing either.
    finger = await press(page, 600, folder)
    await finger.to(firstGap)
    await expect(firstGap).toHaveCSS('background-color', theme)
    await finger.to(bot(2))
    await expect(firstGap).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
    expect(await marked()).toEqual([])
    await finger.to(folderGap)
    await expect(folderGap).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
    await finger.to(gap)
    await expect(gap).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
    await finger.release()
    expect((await order())[0]).toBe('bot-2')
    expect((await order())[1].data).toEqual(['bot-3', 'bot-1'])
    await move(page, 600, folder, folder)
    await expect.poll(() => page.evaluate(() => (window as any).sidebarModel.menus)).toBe(1)
    expect(await page.evaluate(() => (window as any).sidebarModel.clicks)).toBe(0)
    expect(errors).toEqual([])
})
