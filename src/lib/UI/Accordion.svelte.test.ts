import { afterEach, describe, expect, it, vi } from 'vitest'
import { createRawSnippet, mount, tick, unmount } from 'svelte'
import Accordion from './Accordion.svelte'

vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))

const mounted: ReturnType<typeof mount>[] = []
const body = createRawSnippet(() => ({ render: () => '<p>Body</p>' }))

async function show(props: { openKey?: string, startOpen?: boolean, summary?: string }) {
    const target = document.createElement('div')
    document.body.append(target)
    mounted.push(mount(Accordion, { target, props: { name: 'Section', styled: true, children: body, ...props } }))
    await tick()
    return target
}
const header = (target: Element) => target.querySelector('button')!
const isOpen = (target: Element) => target.textContent?.includes('Body') ?? false
async function close(target: Element) {
    const index = [...document.body.children].indexOf(target)
    await unmount(mounted.splice(index, 1)[0])
    target.remove()
}

afterEach(async () => {
    for (const instance of mounted.splice(0)) await unmount(instance)
    document.body.replaceChildren()
})

describe('accordion', () => {
    it('reopens as the user left it under its key, and starts closed without one', async () => {
        const keyed = await show({ openKey: 'accordion-test-remembered' })
        const plain = await show({})
        expect(isOpen(keyed)).toBe(false)
        header(keyed).click()
        header(plain).click()
        await tick()
        await close(keyed)
        await close(plain)
        expect(isOpen(await show({ openKey: 'accordion-test-remembered' }))).toBe(true)
        expect(isOpen(await show({ openKey: 'accordion-test-other' }))).toBe(false)
        expect(isOpen(await show({}))).toBe(false)
    })

    it('starts open when asked, until the user closes it under its key', async () => {
        expect(isOpen(await show({ startOpen: true }))).toBe(true)
        const keyed = await show({ openKey: 'accordion-test-start-open', startOpen: true })
        expect(isOpen(keyed)).toBe(true)
        header(keyed).click()
        await tick()
        await close(keyed)
        expect(isOpen(await show({ openKey: 'accordion-test-start-open', startOpen: true }))).toBe(false)
    })

    it('shows a summary in the header', async () => {
        expect(header(await show({ summary: '3' })).textContent).toContain('3')
        expect(header(await show({})).textContent?.trim()).toBe('Section')
    })
})
