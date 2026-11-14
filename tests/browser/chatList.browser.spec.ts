import { test, expect } from './fixture'
import { devices, type Locator, type Page } from '@playwright/test'
import { touchHoldMs } from '../../src/ts/gui/touchHold'

type Point = { x: number, y: number }

// Sortable attaches after the list renders, and a destroyed one leaves its key behind, empty.
const sortableReady = (page: Page) =>
    page.waitForFunction(() => Object.entries(document.querySelector('.risu-chat') ?? {}).some(([key, sortable]) => key.startsWith('Sortable') && sortable))
async function open(page: Page, query = '') {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(`/chatList.html${query}`)
    await expect.poll(async () => ({ rows: await page.locator('[data-risu-chat-idx]').count() > 0, errors })).toEqual({ rows: true, errors: [] })
    await sortableReady(page)
    return errors
}
/** Runs a drag and waits until the list is rebuilt from what the drag left. */
async function untilRebuilt(page: Page, drag: () => Promise<void>) {
    const list = await page.locator('.risu-chat-list').elementHandle()
    await drag()
    await page.waitForFunction(old => !old!.isConnected, list)
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
/** A folder that goes away while a chat is dragged, as a sync removes one; the list is rebuilt without it. */
const removeFolder = (page: Page) =>
    page.evaluate(() => { (window as any).chatListState.db.characters[0].chatFolders.pop() })
/** What marks a chat as held, as the slot, or as the copy under the pointer. */
const dragMarks = (page: Page) => page.locator('.risu-sort-held, .sortable-chosen, .sortable-ghost, .sortable-fallback')
const folder = (page: Page, name: string) => page.locator('[data-risu-chat-folder-idx]').filter({ hasText: name })
const chat = (page: Page, name: string) => page.locator('[data-risu-chat-idx]').filter({ hasText: name })
async function middle(locator: Locator): Promise<Point> {
    const box = await locator.boundingBox()
    return { x: box!.x + box!.width / 3, y: box!.y + box!.height / 2 }
}

test.describe('chat list by touch', () => {
    test.use({ hasTouch: true, isMobile: true, viewport: { width: 393, height: 852 } })

    /** Holds a chat until it can be dragged, then moves the finger and lifts it, or loses it to the browser, where `during` leaves it. */
    async function touchDrag(page: Page, source: Locator, during: (move: (point: Point) => Promise<void>) => Promise<void>, held?: () => Promise<void>, finish: 'touchEnd' | 'touchCancel' = 'touchEnd') {
        const cdp = await page.context().newCDPSession(page)
        const send = (type: 'touchStart' | 'touchMove' | 'touchEnd' | 'touchCancel', point?: Point) =>
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
        await send(finish)
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

    test('a chat whose touch is cancelled over a folded folder stays where it was', async ({ page }) => {
        await open(page)
        await arrange(page, { 'Chat 1': 'second' }, ['second'])
        const target = folder(page, 'Second folder')
        // The drag ends with the cancelled touch, not with a later one.
        await untilRebuilt(page, () => touchDrag(page, chat(page, 'Chat 3'), async (move) => {
            await move(await middle(target.locator('[data-risu-chat-folder-header]')))
            await expect.poll(() => target.evaluate(element => getComputedStyle(element).outlineStyle)).toBe('solid')
        }, undefined, 'touchCancel'))
        expect(await listed(page)).toEqual({ 'First folder': [], 'Second folder': ['Chat 1'], outside: ['Chat 2', 'Chat 3'] })
    })

    test('a held chat is let go when the list is rebuilt under it, and the list drags again', async ({ page }) => {
        const errors = await open(page)
        await untilRebuilt(page, () => touchDrag(page, chat(page, 'Chat 3'), async (move) => {
            await move(await middle(chat(page, 'Chat 1')))
            await expect(dragMarks(page)).not.toHaveCount(0)
            await removeFolder(page)
            await expect(dragMarks(page)).toHaveCount(0)
        }, undefined, 'touchCancel'))
        expect(await listed(page)).toEqual({ 'First folder': [], outside: ['Chat 1', 'Chat 2', 'Chat 3'] })
        await sortableReady(page)
        await touchDrag(page, chat(page, 'Chat 3'), async move => move(await middle(chat(page, 'Chat 1'))))
        await expect.poll(async () => (await listed(page)).outside).toEqual(['Chat 3', 'Chat 1', 'Chat 2'])
        // The cancelled touch reached no list that was already gone.
        expect(errors).toEqual([])
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

    test.describe('on a phone', () => {
        // Sortable drags the rows itself on Android and never marks them draggable, so nothing else keeps their text from being selected.
        test.use({ userAgent: devices['Pixel 7'].userAgent })

        test('marks the held chat, the slot it lands in, and the copy under the finger', async ({ page }) => {
            await open(page)
            // The copy that follows the finger is the only fixed row.
            const marks = () => chat(page, 'Chat 3').evaluateAll(rows => Object.fromEntries(rows.map(row => {
                const style = getComputedStyle(row)
                return [style.position === 'fixed' ? 'following' : 'listed', {
                    outline: style.outlineStyle,
                    opacity: style.opacity,
                    text: getComputedStyle(row.firstElementChild!).opacity,
                    // A finger at rest on the held chat must not start a text selection, which takes the touch on Android.
                    select: style.userSelect,
                }]
            })))
            expect(await marks()).toEqual({ listed: { outline: 'none', opacity: '1', text: '1', select: 'auto' } })
            await touchDrag(page, chat(page, 'Chat 3'), async (move) => {
                await move(await middle(chat(page, 'Chat 1')))
                await expect.poll(marks).toEqual({
                    listed: { outline: 'dashed', opacity: '1', text: '0.45', select: 'none' },
                    following: { outline: 'solid', opacity: '1', text: '1', select: 'none' },
                })
            }, async () => {
                await expect.poll(marks).toEqual({ listed: { outline: 'solid', opacity: '1', text: '1', select: 'none' } })
            })
            await expect.poll(() => listed(page)).toEqual({ 'First folder': [], 'Second folder': [], outside: ['Chat 3', 'Chat 1', 'Chat 2'] })
        })
    })
})

/** Picks a chat, or the row given, up with the mouse and holds it over the middle of a row. */
async function mouseDragTo(page: Page, source: string | Locator, target: Locator) {
    const start = await middle(typeof source === 'string' ? chat(page, source) : source)
    const over = await middle(target)
    await page.mouse.move(start.x, start.y)
    await page.mouse.down()
    await page.mouse.move(start.x + 8, start.y, { steps: 3 })
    await page.mouse.move(over.x, over.y, { steps: 8 })
    await page.mouse.move(over.x, over.y)
}
const header = (page: Page, name: string) => folder(page, name).locator('[data-risu-chat-folder-header]')

test('a chat dragged by mouse onto a folder header becomes its first chat', async ({ page }) => {
    await open(page)
    await arrange(page, { 'Chat 1': 'first' })
    await mouseDragTo(page, 'Chat 3', header(page, 'First folder'))
    await page.mouse.up()
    await expect.poll(() => listed(page)).toEqual({ 'First folder': ['Chat 3', 'Chat 1'], 'Second folder': [], outside: ['Chat 2'] })
})

test('a chat dragged by mouse onto a folded folder joins it when dropped and stays where it was when the drag is abandoned', async ({ page }) => {
    await open(page)
    await arrange(page, { 'Chat 1': 'second' }, ['second'])
    await untilRebuilt(page, async () => {
        await mouseDragTo(page, 'Chat 3', header(page, 'Second folder'))
        await page.keyboard.press('Escape')
    })
    await page.mouse.up()
    expect(await listed(page)).toEqual({ 'First folder': [], 'Second folder': ['Chat 1'], outside: ['Chat 2', 'Chat 3'] })
    await sortableReady(page)
    await mouseDragTo(page, 'Chat 3', header(page, 'Second folder'))
    await page.mouse.up()
    await expect.poll(() => listed(page)).toEqual({ 'First folder': [], 'Second folder': ['Chat 3', 'Chat 1'], outside: ['Chat 2'] })
})

test('a folder dragged by mouse stays where it was when the drag is abandoned and moves when dropped', async ({ page }) => {
    await open(page)
    const folders = () => page.evaluate(() => (window as any).chatListState.db.characters[0].chatFolders.map((folder: any) => folder.id))
    await untilRebuilt(page, async () => {
        await mouseDragTo(page, header(page, 'Second folder'), header(page, 'First folder'))
        await page.keyboard.press('Escape')
    })
    await page.mouse.up()
    expect(await folders()).toEqual(['first', 'second'])
    await sortableReady(page)
    await untilRebuilt(page, async () => {
        await mouseDragTo(page, header(page, 'Second folder'), header(page, 'First folder'))
        await page.mouse.up()
    })
    expect(await folders()).toEqual(['second', 'first'])
})

test('a chat dragged by mouse beside the list stays where it was when the drag is abandoned and lands in its slot when released', async ({ page }) => {
    await open(page)
    const list = await page.locator('.risu-chat-list').boundingBox()
    await page.evaluate(() => document.addEventListener('drop', () => { (window as any).droppedBeside = true }))
    const dragBeside = async () => {
        const over = await middle(chat(page, 'Chat 2'))
        await mouseDragTo(page, 'Chat 3', chat(page, 'Chat 2'))
        await page.mouse.move(list!.x + list!.width + 60, over.y, { steps: 4 })
    }
    await untilRebuilt(page, async () => {
        await dragBeside()
        await page.keyboard.press('Escape')
    })
    await page.mouse.up()
    expect((await listed(page)).outside).toEqual(['Chat 1', 'Chat 2', 'Chat 3'])
    await sortableReady(page)
    await untilRebuilt(page, async () => {
        await dragBeside()
        await page.mouse.up()
    })
    expect((await listed(page)).outside).toEqual(['Chat 1', 'Chat 3', 'Chat 2'])
    // What lies beside the list is not handed the chat as a drop.
    expect(await page.evaluate(() => (window as any).droppedBeside)).toBeUndefined()
})

test('a chat dragged by mouse is let go when the list is rebuilt under it, and the list drags again', async ({ page }) => {
    const errors = await open(page)
    await untilRebuilt(page, async () => {
        await mouseDragTo(page, 'Chat 3', chat(page, 'Chat 1'))
        await expect(dragMarks(page)).not.toHaveCount(0)
        await removeFolder(page)
    })
    await page.mouse.up()
    expect(await listed(page)).toEqual({ 'First folder': [], outside: ['Chat 1', 'Chat 2', 'Chat 3'] })
    await sortableReady(page)
    await mouseDragTo(page, 'Chat 3', chat(page, 'Chat 2'))
    await page.mouse.up()
    await expect.poll(async () => (await listed(page)).outside).toEqual(['Chat 1', 'Chat 3', 'Chat 2'])
    // The release reached no list that was already gone.
    expect(errors).toEqual([])
})
