import { test, expect } from './fixture'
import type { Locator, Page } from '@playwright/test'
import { languageEnglish as language } from '../../src/lang/en'
import { touchHoldMs } from '../../src/ts/gui/touchHold'

async function order(page: Page, kind: string) {
    return page.evaluate(kind => {
        const db = (window as any).dragState.db
        const items = kind.startsWith('lore') ? db.characters[0].globalLore
            : kind === 'regex' ? db.globalscript : kind === 'trigger' ? db.characters[0].triggerscript
            : kind === 'persona' ? db.personas : db.promptTemplate
        return items.map((item: any) => item.comment ?? item.name)
    }, kind)
}
async function open(page: Page, kind: string) {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(`/dragDrop.html?kind=${kind}`)
    await expect.poll(async () => ({ mounted: await page.locator('#drag-fixture').count(), errors })).toEqual({ mounted: 1, errors: [] })
    return errors
}
async function drag(source: Locator, target: Locator, bottom = false, finish: 'drop' | 'escape' | 'beside' | 'hold' = 'drop') {
    await source.scrollIntoViewIfNeeded()
    const start = await source.boundingBox()
    const box = await target.boundingBox()
    const mouse = source.page().mouse
    await mouse.move(start!.x + start!.width / 2, start!.y + start!.height / 2)
    await mouse.down()
    await mouse.move(start!.x + start!.width / 2 + 8, start!.y + start!.height / 2, { steps: 3 })
    const x = box!.x + box!.width / 2
    const y = box!.y + (bottom ? box!.height - 3 : 3)
    await mouse.move(x, y, { steps: 8 })
    await mouse.move(x, y)
    if (finish === 'escape') await source.page().keyboard.press('Escape')
    if (finish === 'beside') {
        const list = await source.page().locator('[data-show-folder=""]').boundingBox()
        await mouse.move(list!.x + list!.width + 40, y, { steps: 4 })
    }
    if (finish !== 'hold') await mouse.up()
}
type Point = { x: number, y: number }
/** Holds a finger on `source` until it can be dragged, then lifts it, or loses it to the browser, where `during` leaves it. */
async function touchDrag(page: Page, source: Locator, during: (move: (point: Point) => Promise<void>) => Promise<void>, finish: 'touchEnd' | 'touchCancel' = 'touchEnd') {
    const cdp = await page.context().newCDPSession(page)
    const send = (type: 'touchStart' | 'touchMove' | 'touchEnd' | 'touchCancel', point?: Point) =>
        cdp.send('Input.dispatchTouchEvent', { type, touchPoints: point ? [point] : [] })
    await source.scrollIntoViewIfNeeded()
    const box = await source.boundingBox()
    let at = { x: box!.x + box!.width / 2, y: box!.y + box!.height / 2 }
    await send('touchStart', at)
    await page.waitForTimeout(touchHoldMs + 100)
    at = { x: at.x, y: at.y + 4 }
    await send('touchMove', at)
    await page.waitForTimeout(60)
    await during(async (point) => {
        // Cross the rows on the way at a finger's pace.
        for (let step = 1; step <= 6; step++) {
            await send('touchMove', { x: at.x + (point.x - at.x) * step / 6, y: at.y + (point.y - at.y) * step / 6 })
            await page.waitForTimeout(40)
        }
        // A second sample at rest, as a finger that stopped moving gives.
        at = { x: point.x + 1, y: point.y }
        await send('touchMove', at)
        await page.waitForTimeout(120)
    })
    await send(finish)
}
const outline = (locator: Locator) => locator.evaluate(element => getComputedStyle(element).outlineStyle)
const opacity = (locator: Locator) => locator.evaluate(element => Number(getComputedStyle(element).opacity))

for (const kind of ['regex', 'trigger']) {
    test(`${kind}: restores dragging after replacing an open list`, async ({ page }) => {
        await open(page, kind)
        await page.getByRole('button', { name: 'First', exact: true }).click()
        await page.evaluate(kind => {
            const db = (window as any).dragState.db
            if (kind === 'regex') db.globalscript = db.globalscript.slice(1)
            else db.characters[0].triggerscript = db.characters[0].triggerscript.slice(1)
        }, kind)
        await expect(page.getByRole('button', { name: 'First', exact: true })).toHaveCount(0)
        await drag(page.getByRole('button', { name: 'Last', exact: true }), page.getByRole('button', { name: 'Second', exact: true }))
        await expect.poll(() => order(page, kind)).toEqual(['Last', 'Second'])
    })

    test(`${kind}: delete, edit, and repeatedly reorder with the real Sortable library`, async ({ page }) => {
        const errors: string[] = []
        page.on('pageerror', error => errors.push(error.message))
        await open(page, kind)
        await page.getByRole('button', { name: 'First', exact: true }).locator('..').locator('button').nth(1).click()
        await page.getByRole('button', { name: 'Second', exact: true }).click()
        await page.getByRole('button', { name: 'Second', exact: true }).click()
        const last = page.getByRole('button', { name: 'Last', exact: true })
        const second = page.getByRole('button', { name: 'Second', exact: true })
        await drag(last, second)
        await expect.poll(() => order(page, kind)).toEqual(['Last', 'Second'])
        await drag(second, last)
        await expect.poll(() => order(page, kind)).toEqual(['Second', 'Last'])
        expect(errors).toEqual([])
    })

    test(`${kind}: an entry stays where it was when its drag is abandoned`, async ({ page }) => {
        await open(page, kind)
        const second = page.getByRole('button', { name: 'Second', exact: true })
        const last = page.getByRole('button', { name: 'Last', exact: true })
        await drag(last, second, false, 'escape')
        await expect(page.locator('.sortable-chosen')).toHaveCount(0)
        expect(await order(page, kind)).toEqual(['First', 'Second', 'Last'])
        await drag(last, second)
        await expect.poll(() => order(page, kind)).toEqual(['First', 'Last', 'Second'])
    })
}

test('persona: a persona stays where it was when its drag is abandoned and moves when dropped', async ({ page }) => {
    const errors = await open(page, 'persona')
    const persona = (index: number) => page.locator('[data-risu-idx]').nth(index)
    // The list is drawn again after each drag, and Sortable attaches to the new one.
    const redrawn = async (finish: 'drop' | 'escape') => {
        const list = await persona(0).locator('..').elementHandle()
        await drag(persona(2), persona(1), false, finish)
        await page.waitForFunction(old => !old!.isConnected, list)
        await page.waitForFunction(() => Object.keys(document.querySelector('[data-risu-idx]')!.parentElement!).some(key => key.startsWith('Sortable')))
    }
    await redrawn('escape')
    expect(await order(page, 'persona')).toEqual(['First', 'Second', 'Last'])
    await redrawn('drop')
    expect(await order(page, 'persona')).toEqual(['First', 'Last', 'Second'])
    // Closing the page afterwards ends no drag: the one that was dropped is over.
    await page.evaluate(() => (window as any).closeDragFixture())
    expect(errors).toEqual([])
})

for (const kind of ['lore', 'regex', 'trigger', 'persona']) {
    test(`${kind}: a list closed under a dragged row ends the drag`, async ({ page }) => {
        const errors = await open(page, kind)
        const [source, target] = kind === 'persona' ? [page.locator('[data-risu-idx]').nth(2), page.locator('[data-risu-idx]').nth(1)]
            : [itemButton(page, 'Last'), itemButton(page, kind === 'lore' ? 'First' : 'Second')]
        await drag(source, target, false, 'hold')
        await expect(page.locator('.sortable-ghost')).toHaveCount(1)
        await page.evaluate(() => (window as any).closeDragFixture())
        // Nothing takes a later drag over the page, as a file dragged in.
        expect(await page.evaluate(() => {
            const over = new DragEvent('dragover', { bubbles: true, cancelable: true })
            document.body.dispatchEvent(over)
            return over.defaultPrevented
        })).toBe(false)
        await page.mouse.up()
        // The release reaches no list that is gone, and the closed list is not built again.
        await page.waitForTimeout(100)
        expect(errors).toEqual([])
    })
}

test('lore: reorder around an expanded folder without remounting its editors', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await open(page, 'lore')
    await page.getByRole('button', { name: 'Folder', exact: true }).click()
    await page.getByRole('button', { name: 'Child', exact: true }).click()
    const input = await page.locator('[data-show-folder="folder"] input').first().elementHandle()
    await drag(page.getByRole('button', { name: 'Last', exact: true }), page.getByRole('button', { name: 'First', exact: true }))
    await expect.poll(() => order(page, 'lore')).toEqual(['Folder', 'Child', 'Last', 'First'])
    expect(await input!.evaluate(node => node.isConnected)).toBe(true)
    expect(errors).toEqual([])
})

test('lore: move into a folder and back to the root', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await open(page, 'lore')
    await page.getByRole('button', { name: 'Folder', exact: true }).click()
    const first = page.getByRole('button', { name: 'First', exact: true })
    await drag(first, page.getByRole('button', { name: 'Child', exact: true }), true)
    const folderOfFirst = () => page.evaluate(() => (window as any).dragState.db.characters[0].globalLore.find((item: any) => item.comment === 'First').folder)
    await expect.poll(folderOfFirst).toBe('folder')
    await expect(page.locator('[data-show-folder="folder"]').getByRole('button', { name: 'First', exact: true })).toBeVisible()
    await drag(first, page.getByRole('button', { name: 'Last', exact: true }), true)
    await expect.poll(folderOfFirst).toBeUndefined()
    expect(errors).toEqual([])
})

test('lore: an entry dragged onto a closed folder row becomes its first entry', async ({ page }) => {
    await open(page, 'lore')
    await drag(itemButton(page, 'Last'), itemButton(page, 'Folder'))
    await expect.poll(() => order(page, 'lore')).toEqual(['Folder', 'Last', 'Child', 'First'])
})

test('lore: an entry stays where it was when its drag onto a closed folder row is abandoned', async ({ page }) => {
    await open(page, 'lore')
    await drag(itemButton(page, 'Last'), itemButton(page, 'Folder'), false, 'escape')
    await expect(page.locator('.risu-chosen-item')).toHaveCount(0)
    expect(await order(page, 'lore')).toEqual(['Folder', 'Child', 'First', 'Last'])
})

test('lore: an entry dragged out of an open folder stays in it when the drag is abandoned', async ({ page }) => {
    await open(page, 'lore')
    await itemButton(page, 'Folder').click()
    await drag(itemButton(page, 'Child'), itemButton(page, 'Last'), true, 'escape')
    await expect(page.locator('.risu-chosen-item')).toHaveCount(0)
    expect(await page.evaluate(() => (window as any).dragState.db.characters[0].globalLore.map((item: any) => `${item.comment}:${item.folder ?? ''}`)))
        .toEqual(['Folder:', 'Child:folder', 'First:', 'Last:'])
})

test('lore: an entry released beside the list lands in its slot', async ({ page }) => {
    await open(page, 'lore')
    await drag(itemButton(page, 'Last'), itemButton(page, 'First'), false, 'beside')
    await expect.poll(() => order(page, 'lore')).toEqual(['Folder', 'Child', 'Last', 'First'])
})

test('prompt: use gaps and card halves without changing the drag source during hover', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    await open(page, 'prompt')
    const header = (name: string) => page.locator('[draggable="true"]').filter({ has: page.locator('span', { hasText: new RegExp(`^${name}$`) }) })
    const first = header('First')
    await first.click()
    const input = await first.locator('..').locator('input').first().elementHandle()
    await drag(first, header('Last'), true)
    await expect.poll(() => order(page, 'prompt')).toEqual(['Second', 'Last', 'First'])
    expect(await input!.evaluate(node => node.isConnected)).toBe(true)
    await drag(header('First'), page.locator('[role="doc-pagebreak"]').first())
    await expect.poll(() => order(page, 'prompt')).toEqual(['First', 'Second', 'Last'])
    expect(errors).toEqual([])
})

test.describe('touch dragging', () => {
    test.use({ hasTouch: true, isMobile: true, viewport: { width: 393, height: 852 } })
    test('reorders regex entries after the touch hold delay', async ({ page, context }) => {
        await open(page, 'regex')
        const source = await page.getByRole('button', { name: 'Last', exact: true }).boundingBox()
        const target = await page.getByRole('button', { name: 'First', exact: true }).boundingBox()
        const cdp = await context.newCDPSession(page)
        const x = source!.x + source!.width / 2
        const y = source!.y + source!.height / 2
        await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x, y }] })
        await page.waitForTimeout(touchHoldMs + 100)
        for (let step = 1; step <= 6; step++) {
            await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{
                x, y: y + (target!.y + 3 - y) * step / 6,
            }] })
            await page.waitForTimeout(40)
        }
        await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] })
        await expect.poll(() => order(page, 'regex')).toEqual(['Last', 'First', 'Second'])
        // A mouse drags the list the browser's way again once the finger is off: Escape abandons the drag.
        await drag(itemButton(page, 'Second'), itemButton(page, 'First'), false, 'escape')
        await expect(page.locator('.sortable-chosen')).toHaveCount(0)
        expect(await order(page, 'regex')).toEqual(['Last', 'First', 'Second'])
    })

    for (const kind of ['lore', 'regex', 'trigger']) {
        test(`${kind}: a held row is let go when another row is opened under the drag`, async ({ page }) => {
            await open(page, kind)
            const before = await order(page, kind)
            // The keyboard opens a row while the finger holds another one.
            await itemButton(page, 'First').focus()
            await touchDrag(page, itemButton(page, 'Last'), async (move) => {
                const box = await itemButton(page, kind === 'lore' ? 'First' : 'Second').boundingBox()
                await move({ x: box!.x + box!.width / 2, y: box!.y + box!.height - 3 })
                await expect(page.locator('.sortable-fallback')).toHaveCount(1)
                await page.keyboard.press('Enter')
                await expect(page.locator('.sortable-fallback, .sortable-ghost, .risu-sort-held')).toHaveCount(0)
            })
            expect(await order(page, kind)).toEqual(before)
        })
    }

    test('lore: an entry released on a closed folder row becomes its first entry', async ({ page }) => {
        await open(page, 'lore')
        const folderRow = loreRow(page, 'Folder')
        await touchDrag(page, itemButton(page, 'Last'), async (move) => {
            await move(await center(itemButton(page, 'Folder')))
            await expect.poll(() => outline(folderRow)).toBe('solid')
            expect(await outline(loreRow(page, 'First'))).toBe('none')
            // The folder and its mark show through the copy under the finger.
            expect(await opacity(page.locator('.sortable-fallback'))).toBeLessThan(1)
        })
        await expect.poll(() => order(page, 'lore')).toEqual(['Folder', 'Last', 'Child', 'First'])
        await expect.poll(() => outline(folderRow)).toBe('none')
    })

    test('lore: an entry stays where it was when its touch is cancelled on a closed folder row', async ({ page }) => {
        await open(page, 'lore')
        const folderRow = loreRow(page, 'Folder')
        await touchDrag(page, itemButton(page, 'Last'), async (move) => {
            await move(await center(itemButton(page, 'Folder')))
            await expect.poll(() => outline(folderRow)).toBe('solid')
        }, 'touchCancel')
        // The drag ends with the cancelled touch, not with a later one.
        await expect(page.locator('.sortable-fallback')).toHaveCount(0)
        expect(await order(page, 'lore')).toEqual(['Folder', 'Child', 'First', 'Last'])
    })

    test('lore: an entry released on an open folder name becomes its first entry', async ({ page }) => {
        await open(page, 'lore')
        await itemButton(page, 'Folder').click()
        await expect(page.locator('[data-show-folder="folder"]').getByRole('button', { name: 'Child', exact: true })).toBeVisible()
        const folderRow = loreRow(page, 'Folder')
        await itemButton(page, 'Last').scrollIntoViewIfNeeded()
        const start = await center(itemButton(page, 'Last'))
        const list = await page.locator('[data-show-folder=""]').boundingBox()
        expect(list!.x).toBeGreaterThan(4)
        // Pass beside the list, so the entry keeps its place at the root until the finger is on the folder name.
        const beside = list!.x / 2
        await touchDrag(page, itemButton(page, 'Last'), async (move) => {
            const name = await center(itemButton(page, 'Folder'))
            await move({ x: beside, y: start.y + 4 })
            await move({ x: beside, y: name.y })
            expect(await outline(folderRow)).toBe('none')
            expect(await opacity(page.locator('.sortable-fallback'))).toBe(1)
            await move(name)
            await expect.poll(() => outline(folderRow)).toBe('solid')
            expect(await outline(loreRow(page, 'First'))).toBe('none')
        })
        await expect.poll(() => order(page, 'lore')).toEqual(['Folder', 'Last', 'Child', 'First'])
        await expect(page.locator('[data-show-folder="folder"]').getByRole('button', { name: 'Last', exact: true })).toBeVisible()
        await expect.poll(() => outline(folderRow)).toBe('none')
    })

    test('prompt: a held item header reorders the items', async ({ page }) => {
        await open(page, 'prompt')
        const first = promptHeader(page, 'First')
        await first.scrollIntoViewIfNeeded()
        await touchDrag(page, promptHeader(page, 'Last').locator('span').first(), async (move) => {
            const box = await first.boundingBox()
            await move({ x: box!.x + box!.width / 3, y: box!.y + 3 })
            // The item stays in place, marked like a slot, under a card the drop marker shows through.
            const source = promptHeader(page, 'Last').first()
            await expect(source).toHaveClass(/\brisu-drag-source\b/)
            expect(await outline(source)).toBe('dashed')
            expect(await opacity(page.locator('.risu-drag-card'))).toBe(0.7)
        })
        await expect.poll(() => order(page, 'prompt')).toEqual(['Last', 'First', 'Second'])
    })
})

async function center(locator: Locator) {
    await expect(locator).toBeVisible()
    const box = await locator.boundingBox()
    return { x: box!.x + box!.width / 2, y: box!.y + box!.height / 2, top: box!.y + 3 }
}
const zone = (page: Page, direction: 'previous' | 'next') => page.locator(`[data-lore-page-zone="${direction}"]`)
const pagerText = (page: Page) => page.getByRole('navigation', { name: language.risuNest.pager.pages }).first()
const itemButton = (page: Page, name: string) => page.getByRole('button', { name, exact: true })
const loreRow = (page: Page, name: string) => itemButton(page, name).locator('xpath=../..')
const promptHeader = (page: Page, name: string) =>
    page.locator('[draggable="true"]').filter({ has: page.locator('span', { hasText: new RegExp(`^${name}$`) }) })
async function position(page: Page, name: string) {
    return (await order(page, 'lore')).indexOf(name)
}

/** Hovers the top edge of a row at a pace Sortable's move animation keeps up with. */
async function settleOn(page: Page, target: string, via?: string) {
    // Leave the repeat zone before waiting for the drop target to settle.
    if (via) {
        const pass = await center(itemButton(page, via).locator('..'))
        await page.mouse.move(pass.x, pass.y, { steps: 6 })
        // A delayed drop must stay on this page past the 700 ms repeat interval.
        await page.waitForTimeout(750)
    }
    // Rows shift once the dragged row leaves its pinned edge, so measure the target after that.
    const drop = await center(itemButton(page, target).locator('..'))
    await page.mouse.move(drop.x, drop.top, { steps: 6 })
    await page.waitForTimeout(250)
    await page.mouse.move(drop.x, drop.top)
}

test.describe('lore pages', () => {
    test.use({ viewport: { width: 800, height: 6400 } })

    async function dragThroughZone(page: Page, source: string, direction: 'previous' | 'next', expected: string, target: string, via?: string) {
        const start = await center(itemButton(page, source).locator('..'))
        const mouse = page.mouse
        await mouse.move(start.x, start.y)
        await mouse.down()
        await mouse.move(start.x + 8, start.y, { steps: 3 })
        // The zone moves when the dragged row enters this list, so aim again once it settles.
        for (let attempt = 0; attempt < 2; attempt++) {
            const edge = await center(zone(page, direction))
            await mouse.move(edge.x, edge.y, { steps: 8 })
            await page.waitForTimeout(200)
        }
        await expect(pagerText(page)).toContainText(expected)
        await settleOn(page, target, via)
        await expect(pagerText(page)).toContainText(expected)
        await mouse.up()
    }

    test('hovering the next-page zone carries a row to a later page', async ({ page }) => {
        const errors: string[] = []
        page.on('pageerror', error => errors.push(error.message))
        await open(page, 'lore-pages')
        await expect(zone(page, 'next')).toHaveCount(0)
        // An open folder on the page left behind goes away under the drag, which carries on.
        await itemButton(page, 'Folder').click()
        const folderList = await page.locator('[data-show-folder="folder"]').elementHandle()
        await dragThroughZone(page, 'Item 0', 'next', '2 / 3', 'Item 70', 'Item 100')
        await expect.poll(async () => (await position(page, 'Item 70')) - (await position(page, 'Item 0'))).toBe(1)
        await expect(itemButton(page, 'Item 0')).toBeVisible()
        await expect(zone(page, 'next')).toHaveCount(0)
        // Sortable lets go of the folder's list once the drag is over, and a later drag does not reach for it again.
        await expect.poll(() => folderList!.evaluate(list => Object.entries(list).some(([key, sortable]) => key.startsWith('Sortable') && sortable))).toBe(false)
        // Sortable takes no new drag while the rows still slide into place from the drop.
        await expect(page.locator('[data-risu-idx][style*="transform"]')).toHaveCount(0)
        await drag(itemButton(page, 'Item 0'), itemButton(page, 'Item 70'), true)
        await expect.poll(async () => (await position(page, 'Item 0')) - (await position(page, 'Item 70'))).toBe(1)
        expect(errors).toEqual([])
    })

    test('hovering the previous-page zone carries a row to an earlier page', async ({ page }) => {
        const errors: string[] = []
        page.on('pageerror', error => errors.push(error.message))
        await open(page, 'lore-pages')
        await page.getByRole('button', { name: language.risuNest.pager.next }).first().click()
        await dragThroughZone(page, 'Item 100', 'previous', '1 / 3', 'Item 5')
        await expect.poll(async () => (await position(page, 'Item 5')) - (await position(page, 'Item 100'))).toBe(1)
        await expect(itemButton(page, 'Item 100')).toBeVisible()
        expect(errors).toEqual([])
    })

    test('a row dragged out of a folder survives the root page change', async ({ page }) => {
        const errors: string[] = []
        page.on('pageerror', error => errors.push(error.message))
        await open(page, 'lore-pages')
        await itemButton(page, 'Folder').click()
        await dragThroughZone(page, 'Child', 'next', '2 / 3', 'Item 70', 'Item 100')
        const child = () => page.evaluate(() => (window as any).dragState.db.characters[0].globalLore.find((item: any) => item.comment === 'Child').folder)
        await expect.poll(child).toBeUndefined()
        await expect.poll(async () => (await position(page, 'Item 70')) - (await position(page, 'Child'))).toBe(1)
        await expect(itemButton(page, 'Child')).toBeVisible()
        expect(errors).toEqual([])
    })
})

test.describe('lore pages by touch', () => {
    test.use({ hasTouch: true, isMobile: true, viewport: { width: 393, height: 6400 } })
    test('holding a touch drag on the next-page zone carries a row to a later page', async ({ page, context }) => {
        const errors: string[] = []
        page.on('pageerror', error => errors.push(error.message))
        await open(page, 'lore-pages')
        const start = await center(itemButton(page, 'Item 0'))
        const cdp = await context.newCDPSession(page)
        const touch = (type: 'touchStart' | 'touchEnd' | 'touchMove' | 'touchCancel', x: number, y: number) =>
            cdp.send('Input.dispatchTouchEvent', { type, touchPoints: type === 'touchEnd' ? [] : [{ x, y }] })
        await touch('touchStart', start.x, start.y)
        await page.waitForTimeout(touchHoldMs + 100)
        await touch('touchMove', start.x, start.y + 10)
        await expect(zone(page, 'next')).toHaveCount(1)
        const edge = await center(zone(page, 'next'))
        for (let step = 1; step <= 6; step++) {
            await touch('touchMove', edge.x, start.y + (edge.y - start.y) * step / 6)
            await page.waitForTimeout(40)
        }
        await expect(pagerText(page)).toContainText('2 / 3')
        await page.waitForTimeout(250)
        const pass = await center(itemButton(page, 'Item 100'))
        for (let step = 1; step <= 6; step++) {
            await touch('touchMove', pass.x, edge.y + (pass.y - edge.y) * step / 6)
            await page.waitForTimeout(60)
        }
        await page.waitForTimeout(250)
        const drop = await center(itemButton(page, 'Item 70'))
        for (let step = 1; step <= 6; step++) {
            await touch('touchMove', drop.x, pass.y + (drop.top - pass.y) * step / 6)
            await page.waitForTimeout(60)
        }
        await page.waitForTimeout(250)
        await touch('touchEnd', 0, 0)
        await expect.poll(async () => (await position(page, 'Item 70')) - (await position(page, 'Item 0'))).toBe(1)
        expect(errors).toEqual([])
    })
})
