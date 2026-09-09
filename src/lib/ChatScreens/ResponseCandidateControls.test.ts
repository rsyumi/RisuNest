import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { languageEnglish } from '../../lang/en'
vi.mock('src/lang', () => ({ language: languageEnglish }))
import Controls from './ResponseCandidateControls.svelte'
let instance: ReturnType<typeof mount>
afterEach(async () => {
    await unmount(instance)
    document.body.replaceChildren()
})
it('separates next and generation, displays position and disables only previous at the first candidate', async () => {
    const previous = vi.fn(),
        next = vi.fn(),
        generate = vi.fn()
    instance = mount(Controls, {
        target: document.body,
        props: { currentPage: 1, totalPages: 3, previous, next, generate },
    })
    await tick()
    const buttons = document.querySelectorAll('button')
    expect(buttons[0].disabled).toBe(true)
    expect(document.querySelector('span')?.textContent).toBe('1/3')
    buttons[1].click()
    buttons[2].click()
    expect(next).toHaveBeenCalledOnce()
    expect(generate).toHaveBeenCalledOnce()
    expect(previous).not.toHaveBeenCalled()
})
it('blocks all candidate actions during generation', async () => {
    instance = mount(Controls, {
        target: document.body,
        props: {
            currentPage: 2,
            totalPages: 2,
            busy: true,
            previous: vi.fn(),
            next: vi.fn(),
            generate: vi.fn(),
        },
    })
    await tick()
    expect([...document.querySelectorAll('button')].every((button) => button.disabled)).toBe(true)
})
it('keeps greeting navigation separate from response generation', async () => {
    instance = mount(Controls, {
        target: document.body,
        props: { greeting: true, previous: vi.fn(), next: vi.fn(), generate: vi.fn() },
    })
    await tick()
    expect(document.querySelectorAll('button')).toHaveLength(2)
    expect(document.querySelector('button')?.disabled).toBe(false)
})
