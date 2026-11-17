import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import PersonaBind from './PersonaBind.svelte'

const mocks = vi.hoisted(() => ({
    bind: vi.fn(async () => {}),
    save: vi.fn(async () => {}),
    list: vi.fn(),
    blocked: vi.fn(() => false),
}))
vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0) }
})
vi.mock('src/ts/alert', () => ({ alertError: vi.fn() }))
vi.mock('src/ts/chatBindings.svelte', () => ({
    captureChatBindingTarget: () => ({
        conversation: DBState.db.characters[0].chats[0],
        isCurrent: () => true,
    }),
    bindPersona: mocks.bind,
    saveChatBinding: mocks.save,
    chatBindingBlockedByGeneration: mocks.blocked,
}))
vi.mock('../Setting/listedPersona.svelte', () => ({
    default: (anchor: unknown, props: Record<string, unknown>) => mocks.list(props),
}))

let instance: ReturnType<typeof mount>
const chat = () => DBState.db.characters[0].chats[0]
const button = () => document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.personaBinding}"]`)!

beforeEach(async () => {
    DBState.db = {
        username: 'Bobby',
        userNote: 'current note',
        selectedPersona: 1,
        personas: [{ id: 'p1', name: 'Alice', note: 'note' }, { id: 'p2', name: 'Bob' }],
        characters: [{ chatPage: 0, chats: [{ id: 'chat' }] }],
    } as unknown as typeof DBState.db
    instance = mount(PersonaBind, { target: document.body })
    await tick()
})

afterEach(async () => {
    await unmount(instance)
    document.body.replaceChildren()
    vi.clearAllMocks()
})

it('shows the header, the current persona with its note while unbound and the bound persona with its own note', async () => {
    expect(document.body.textContent).toContain(languageEnglish.personaBinding)
    expect(button().textContent).toContain(`${languageEnglish.inheritPersona} (Bobby)`)
    expect(button().textContent).toContain('(current note)')
    expect(button().classList.contains('border-selected')).toBe(false)
    chat().bindedPersona = 'p1'
    await tick()
    expect(button().textContent).toContain('Alice')
    expect(button().textContent).toContain('(note)')
    expect(button().textContent).not.toContain('current note')
    expect(button().classList.contains('border-selected')).toBe(true)
    chat().bindedPersona = 'p2'
    await tick()
    expect(button().textContent?.trim()).toBe('Bob')
    chat().bindedPersona = 'missing'
    await tick()
    expect(button().textContent?.trim()).toBe(languageEnglish.missingBoundPersona)
    chat().bindedPersona = undefined
    DBState.db.userNote = ''
    await tick()
    expect(button().textContent?.trim()).toBe(`${languageEnglish.inheritPersona} (Bobby)`)
})

it('opens the persona list in binding mode and persists the selection', async () => {
    button().click()
    await tick()
    expect(mocks.list).toHaveBeenCalledTimes(1)
    const props = mocks.list.mock.calls[0][0] as { bindingMode: boolean; onSelect: (index: number) => Promise<void> }
    expect(props.bindingMode).toBe(true)
    await props.onSelect(0)
    expect(mocks.bind).toHaveBeenCalledWith(chat(), 0)
    expect(mocks.save).toHaveBeenCalledTimes(1)
})

it('does not open or apply a persona binding while a response is generating', async () => {
    mocks.blocked.mockReturnValueOnce(true)
    button().click()
    await tick()
    expect(mocks.list).not.toHaveBeenCalled()

    button().click()
    await tick()
    const props = mocks.list.mock.calls[0][0] as { onSelect: (index: number) => Promise<void> }
    mocks.blocked.mockReturnValueOnce(true)
    await props.onSelect(0)
    expect(mocks.bind).not.toHaveBeenCalled()
    expect(mocks.save).not.toHaveBeenCalled()
})
