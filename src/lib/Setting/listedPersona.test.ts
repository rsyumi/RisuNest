import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { languageEnglish } from '../../lang/en'
const mocks = vi.hoisted(() => ({ change: vi.fn(), select: vi.fn(), close: vi.fn() }))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: {
        db: {
            personas: [
                { id: 'a', name: 'Persona A' },
                { id: 'b', name: 'Persona B' },
            ],
            selectedPersona: 0,
            username: 'Persona A',
        },
    },
}))
vi.mock('../../lang', () => ({ language: languageEnglish }))
vi.mock('src/ts/persona', () => ({ changeUserPersona: mocks.change }))
import ListedPersona from './listedPersona.svelte'
let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    document.body.replaceChildren()
    vi.clearAllMocks()
})
it('binds a persona through the chat callback without changing global selection', async () => {
    instance = mount(ListedPersona, {
        target: document.body,
        props: { bindingMode: true, selectedId: 'b', onSelect: mocks.select, close: mocks.close },
    })
    const button = [...document.querySelectorAll('button')].find((button) =>
        button.textContent?.includes('Persona B'),
    )!
    expect(button.classList.contains('bg-selected')).toBe(true)
    button.click()
    await tick()
    expect(mocks.select).toHaveBeenCalledWith(1)
    expect(mocks.change).not.toHaveBeenCalled()
})
it('offers inheritance only in chat binding mode', async () => {
    instance = mount(ListedPersona, {
        target: document.body,
        props: { bindingMode: true, onSelect: mocks.select, close: mocks.close },
    })
    const button = [...document.querySelectorAll('button')].find((button) =>
        button.textContent?.startsWith(languageEnglish.inheritPersona),
    )!
    expect(button.textContent).toBe(`${languageEnglish.inheritPersona} (Persona A)`)
    button.click()
    await tick()
    expect(mocks.select).toHaveBeenCalledWith(-1)
})
