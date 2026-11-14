import { test, expect } from './fixture'
import type { Locator, Page } from '@playwright/test'
import { touchHoldMs } from '../../src/ts/gui/touchHold'

type Point = { x: number, y: number }

async function open(page: Page, query = '') {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(`/chatList.html${query}`)
    await expect.poll(async () => ({ rows: await page.locator('[data-risu-chat-idx]').count() > 0, errors })).toEqual({ rows: true, errors: [] })
    // Sortable attaches after the list renders.
    await page.waitForFunction(() => Object.keys(document.querySelector('.risu-chat') ?? {}).some(key => key.startsWith('Sortable')))
}
function arrange(page: Page, folders: Record<string, string>, folded: string[] = []) {
    return page.evaluate(({ folders, folded }) => {
        const selected = (window as any).chatListState.db.characters[0]
        for (const chat of selected.chats) chat.folderId = folders[chat.name] ?? null
        for (const folder of selected.chatFolders) folder.folded = folded.includes(folder.id)
    }, { folders, folded })
}
/** What the list shows: each folder's chats in order, then the chats outside folders. */
function listed(page: Page) {
    return page.evaluate(() => {
        const names = (root: Element) => [...root.querySelectorAll('[data-risu-chat-idx]')].map(row => row.querySelector('span')!.textContent)
        const shown: Record<string, (string | null)[]> = {}
        for (const folder of document.querySelectorAll('[data-risu-chat-folder-idx]')) shown[folder.querySelector('span')!.textContent!] = names(folder)
        shown.outside = names(document.querySelector('.risu-chat-ungrouped')!)
        return shown
    })
}
const folder = (page: Page, name: string) => page.locator('[data-risu-chat-folder-idx]').filter({ hasText: name })
const chat = (page: Page, name: string) => page.locator('[data-risu-chat-idx]').filter({ hasText: name })
async function middle(locator: Locator): Promise<Point> {
    const box = await locator.boundingBox()
    return { x: box!.x + box!.width / 3, y: box!.y + box!.height / 2 }
}

test.describe('chat list by touch', () => {
    test.use({ hasTouch: true, isMobile: true, viewport: { width: 393, height: 852 } })

    /** Holds a chat until it can be dragged, then moves the finger and lifts it where `during` leaves it. */
    async function touchDrag(page: Page, source: Locator, during: (move: (point: Point) => Promise<void>) => Promise<void>, held?: () => Promise<void>) {
        const cdp = await page.context().newCDPSession(page)
        const send = (type: 'touchStart' | 'touchMove' | 'touchEnd', point?: Point) =>
            cdp.send('Input.dispatchTouchEvent', { type, touchPoints: point ? [point] : [] })
        const start = await middle(source)
        await send('touchStart', start)
        await page.waitForTimeout(touchHoldMs + 100)
        await held?.()
        await send('touchMove', { x: start.x, y: start.y + 4 })
        await page.waitForTimeout(60)
        await during(async (point) => {
            await send('touchMove', point)
            await page.waitForTimeout(60)
            // A second sample at rest, as a finger that stopped moving gives.
            await send('touchMove', { x: point.x + 1, y: point.y })
            await page.waitForTimeout(120)
        })
        await send('touchEnd')
    }

    test('an empty folder takes a chat dropped anywhere on it', async ({ page }) => {
        await open(page)
        const target = folder(page, 'Second folder')
        const area = await middle(target.locator('.risu-chat'))
        await touchDrag(page, chat(page, 'Chat 3'), async (move) => {
            await move(area)
            // A chat that lands inside the folder, not on its header, keeps an opaque copy.
            await expect.poll(() => target.evaluate(element => getComputedStyle(element).outlineStyle)).toBe('solid')
            expect(await page.locator('.sortable-fallback').evaluate(element => getComputedStyle(element).opacity)).toBe('1')
        })
        await expect.poll(() => listed(page)).toEqual({ 'First folder': [], 'Second folder': ['Chat 3'], outside: ['Chat 1', 'Chat 2'] })
    })

    test('a chat dropped on a folded folder becomes its first chat', async ({ page }) => {
        await open(page)
        await arrange(page, { 'Chat 1': 'second' }, ['second'])
        const target = folder(page, 'Second folder')
        await touchDrag(page, chat(page, 'Chat 3'), async (move) => {
            await move(await middle(target.locator('[data-risu-chat-folder-header]')))
            await expect.poll(() => target.evaluate(element => getComputedStyle(element).outlineStyle)).toBe('solid')
            expect(await folder(page, 'First folder').evaluate(element => getComputedStyle(element).outlineStyle)).toBe('none')
            // The folder and its mark show through the copy under the finger.
            expect(Number(await page.locator('.sortable-fallback').evaluate(element => getComputedStyle(element).opacity))).toBeLessThan(1)
        })
        await expect.poll(() => listed(page)).toEqual({ 'First folder': [], 'Second folder': ['Chat 3', 'Chat 1'], outside: ['Chat 2'] })
        await expect(target.locator('.risu-chat')).toBeHidden()
    })

    test('a chat leaves the folders when none is outside them', async ({ page }) => {
        await open(page)
        await arrange(page, { 'Chat 1': 'first', 'Chat 2': 'second', 'Chat 3': 'second' })
        const last = await folder(page, 'Second folder').boundingBox()
        await touchDrag(page, chat(page, 'Chat 2'), move => move({ x: last!.x + 80, y: last!.y + last!.height + 30 }))
        await expect.poll(() => listed(page)).toEqual({ 'First folder': ['Chat 1'], 'Second folder': ['Chat 3'], outside: ['Chat 2'] })
    })

    test('a drag held at the edge scrolls the list to a folder out of view', async ({ page }) => {
        await open(page, '?chats=16')
        const list = page.locator('.risu-chat-list')
        await list.evaluate(element => { element.scrollTop = element.scrollHeight })
        const edge = await list.boundingBox()
        await touchDrag(page, chat(page, 'Chat 14'), async (move) => {
            await move({ x: edge!.x + 80, y: edge!.y + 10 })
            await expect.poll(() => list.evaluate(element => element.scrollTop)).toBe(0)
            // The first folder's header is now under the resting finger.
            await expect.poll(() => folder(page, 'First folder').evaluate(element => getComputedStyle(element).outlineStyle)).toBe('solid')
        })
        await expect.poll(async () => (await listed(page))['First folder']).toEqual(['Chat 14'])
    })

    test('marks the held chat, the slot it lands in, and the copy under the finger', async ({ page }) => {
        await open(page)
        // The copy that follows the finger is the only fixed row.
        const marks = () => chat(page, 'Chat 3').evaluateAll(rows => Object.fromEntries(rows.map(row => {
            const style = getComputedStyle(row)
            return [style.position === 'fixed' ? 'following' : 'listed', {
                outline: style.outlineStyle,
                opacity: style.opacity,
                text: getComputedStyle(row.firstElementChild!).opacity,
            }]
        })))
        await touchDrag(page, chat(page, 'Chat 3'), async (move) => {
            await move(await middle(chat(page, 'Chat 1')))
            await expect.poll(marks).toEqual({
                listed: { outline: 'dashed', opacity: '1', text: '0.45' },
                following: { outline: 'solid', opacity: '1', text: '1' },
            })
        }, async () => {
            await expect.poll(marks).toEqual({ listed: { outline: 'solid', opacity: '1', text: '1' } })
        })
        await expect.poll(() => listed(page)).toEqual({ 'First folder': [], 'Second folder': [], outside: ['Chat 3', 'Chat 1', 'Chat 2'] })
    })
})

test('a chat dragged by mouse onto a folder header becomes its first chat', async ({ page }) => {
    await open(page)
    await arrange(page, { 'Chat 1': 'first' })
    const start = await middle(chat(page, 'Chat 3'))
    const header = await middle(folder(page, 'First folder').locator('[data-risu-chat-folder-header]'))
    await page.mouse.move(start.x, start.y)
    await page.mouse.down()
    await page.mouse.move(start.x + 8, start.y, { steps: 3 })
    await page.mouse.move(header.x, header.y, { steps: 8 })
    await page.mouse.move(header.x, header.y)
    await page.mouse.up()
    await expect.poll(() => listed(page)).toEqual({ 'First folder': ['Chat 3', 'Chat 1'], 'Second folder': [], outside: ['Chat 2'] })
})
