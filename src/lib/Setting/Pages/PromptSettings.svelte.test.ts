import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { DBState } from 'src/ts/stores.svelte'
import { RISU_PROMPT_DRAG_TYPE } from 'src/ts/dragTypes'
import { stubDragEvent, touch } from 'src/ts/gui/htmlTouchDrag.testSupport'
import { language } from 'src/lang'
import { modalNavigation } from 'src/ts/ui/modalNavigation'
import PromptSettings from './PromptSettings.svelte'

const mocks = vi.hoisted(() => ({
    confirm: vi.fn(async (_message: string) => true),
    toggleEditor: { props: {} as { value?: unknown, onChange?: (value: string) => void } },
}))

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, alertStore: writable({ type: 'none', msg: '' }) }
})
vi.mock('src/ts/process/prompt', () => ({ tokenizePreset: vi.fn(async () => 0) }))
vi.mock('src/ts/process/templates/templateCheck', () => ({ templateCheck: () => [] }))
vi.mock('src/ts/alert', () => ({ alertConfirm: mocks.confirm }))
vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/UI/ModelList.svelte', () => ({ default: () => {} }))
vi.mock('./Model/AuxModelSelectors.svelte', () => ({ default: () => {} }))
// The editor itself is covered by ToggleDefinitionEditor.svelte.test.ts; here it exposes what it receives.
vi.mock('src/lib/UI/ToggleDefinitionEditor.svelte', () => ({
    default: (_anchor: unknown, props: typeof mocks.toggleEditor.props) => { mocks.toggleEditor.props = props },
}))

let instance: ReturnType<typeof mount>
function header(name: string) {
    return [...document.querySelectorAll<HTMLElement>('[draggable="true"]')]
        .find(element => element.querySelector('button[aria-expanded] span span')?.textContent === name)!
}
const toggle = (name: string) => header(name).querySelector<HTMLButtonElement>('button[aria-expanded]')!
function transfer() {
    const data = new Map<string, string>()
    return { get types() { return [...data.keys()] }, setData: (key: string, value: string) => data.set(key, value),
        getData: (key: string) => data.get(key) ?? '', setDragImage: vi.fn() }
}
async function dragEvent(element: Element, type: string, dataTransfer: ReturnType<typeof transfer>, clientY = 0) {
    const event = new Event(type, { bubbles: true, cancelable: true })
    Object.assign(event, { dataTransfer, clientY })
    element.dispatchEvent(event)
    await tick()
    return event
}
beforeEach(async () => {
    DBState.db = { promptTemplate: ['First', 'Second', 'Last'].map(name => ({
        name, type: 'plain', text: '', role: 'system', type2: 'normal',
    })), promptSettings: {} } as any
    instance = mount(PromptSettings, { target: document.body })
    await tick()
})
afterEach(async () => {
    await unmount(instance)
    document.body.replaceChildren()
})

describe('prompt drag ordering', () => {
    it('keeps the same prompt open when another operation inserts an earlier item', async () => {
        toggle('Second').click()
        await tick()
        const input = header('Second').parentElement!.querySelector('input')!
        DBState.db.promptTemplate.unshift({ type: 'plain', name: 'Inserted', text: '', role: 'system', type2: 'normal' })
        await tick()
        expect(header('Second').parentElement!.querySelector('input')).toBe(input)
        expect(header('First').parentElement!.querySelector('input')).toBeNull()
    })
    it('drops before the gap being hovered, independent of the previous card hover', async () => {
        const data = transfer()
        await dragEvent(header('Last'), 'dragstart', data)
        const gap = header('First').parentElement!.previousElementSibling!
        // Drag implementations that follow the HTML specification drop only where dragenter was cancelled.
        expect((await dragEvent(gap, 'dragenter', data)).defaultPrevented).toBe(true)
        expect((await dragEvent(header('Second').parentElement!, 'dragenter', data)).defaultPrevented).toBe(true)
        await dragEvent(gap, 'dragover', data)
        await dragEvent(gap, 'drop', data)
        expect(DBState.db.promptTemplate.map(item => item.name)).toEqual(['Last', 'First', 'Second'])
    })
    it('moves a held header by touch to the gap it is dropped on', async () => {
        const gap = header('First').parentElement!.previousElementSibling!
        const grip = header('Last').querySelector('span')!
        vi.useFakeTimers()
        stubDragEvent()
        vi.spyOn(HTMLElement.prototype, 'getClientRects').mockReturnValue([{}] as unknown as DOMRectList)
        vi.spyOn(document, 'elementFromPoint').mockReturnValue(gap)
        try {
            touch(grip, 'touchstart', 30, 200)
            vi.advanceTimersByTime(450)
            touch(grip, 'touchmove', 30, 20)
            touch(grip, 'touchend', 30, 20)
            await tick()
            expect(DBState.db.promptTemplate.map(item => item.name)).toEqual(['Last', 'First', 'Second'])
        } finally {
            vi.runOnlyPendingTimers()
            vi.useRealTimers()
            vi.unstubAllGlobals()
            vi.restoreAllMocks()
        }
    })
    it('keeps the source and open editors stable during hover, drop, and subsequent editing', async () => {
        const first = header('First')
        toggle('First').click()
        await tick()
        const input = first.parentElement!.querySelector('input')!
        const data = transfer()
        await dragEvent(first, 'dragstart', data)
        await dragEvent(header('Last').parentElement!, 'dragover', data, 20)
        expect(header('First')).toBe(first)
        expect(first.parentElement!.querySelector('input')).toBe(input)
        expect(DBState.db.promptTemplate.map(item => item.name)).toEqual(['First', 'Second', 'Last'])
        await dragEvent(header('Last').parentElement!, 'drop', data, 20)
        expect(DBState.db.promptTemplate.map(item => item.name)).toEqual(['Second', 'Last', 'First'])
        expect(header('First')).toBe(first)
        expect(first.parentElement!.querySelector('input')).toBe(input)
        input.value = 'Edited'
        input.dispatchEvent(new Event('input', { bubbles: true }))
        await tick()
        expect(DBState.db.promptTemplate.map(item => item.name)).toEqual(['Second', 'Last', 'Edited'])
    })
    it('ignores unrelated drags and resets a cancelled prompt drag', async () => {
        const data = transfer()
        data.setData('text/plain', 'unrelated')
        expect((await dragEvent(header('First').parentElement!, 'dragenter', data)).defaultPrevented).toBe(false)
        await dragEvent(header('First').parentElement!, 'drop', data)
        expect(data.types).not.toContain(RISU_PROMPT_DRAG_TYPE)
        await dragEvent(header('Last'), 'dragstart', data)
        await dragEvent(header('First').parentElement!, 'dragover', data)
        await dragEvent(header('Last'), 'dragend', data)
        expect(DBState.db.promptTemplate.map(item => item.name)).toEqual(['First', 'Second', 'Last'])
        expect(document.querySelector('.opacity-50')).toBeNull()
    })
    it('does not reuse a previous target when the prompt is dropped back on itself', async () => {
        const data = transfer()
        await dragEvent(header('First'), 'dragstart', data)
        await dragEvent(header('Last').parentElement!, 'dragover', data, 20)
        await dragEvent(header('First').parentElement!, 'dragover', data, 20)
        await dragEvent(header('First').parentElement!, 'drop', data, 20)
        expect(DBState.db.promptTemplate.map(item => item.name)).toEqual(['First', 'Second', 'Last'])
    })
})

describe('prompt list', () => {
    const names = () => DBState.db.promptTemplate.map(item => item.name)
    const buttonWithText = (text: string) => [...document.querySelectorAll('button')].find(button => button.textContent?.trim() === text)!
    const menuItems = () => [...document.querySelectorAll('[role="menuitem"]')].map(item => item.textContent)

    it('asks before removing a block, naming it or its type when it has no name', async () => {
        DBState.db.promptTemplate[1].name = ''
        await tick()
        mocks.confirm.mockResolvedValueOnce(false)
        header(language.formating.plain).querySelector<HTMLButtonElement>(`button[aria-label="${language.remove}"]`)!.click()
        await vi.waitFor(() => expect(mocks.confirm).toHaveBeenCalledWith(language.removeConfirm + language.formating.plain))
        await tick()
        expect(names()).toEqual(['First', '', 'Last'])
        header('Last').querySelector<HTMLButtonElement>(`button[aria-label="${language.remove}"]`)!.click()
        await vi.waitFor(() => expect(names()).toEqual(['First', '']))
        expect(mocks.confirm).toHaveBeenLastCalledWith(language.removeConfirm + 'Last')
    })

    it('keeps the text and role when switching among plain, jailbreak and chain of thought', async () => {
        DBState.db.promptSettings.customChainOfThought = true
        Object.assign(DBState.db.promptTemplate[0], { text: 'Synthetic text', role: 'user' })
        toggle('First').click()
        await tick()
        const select = (title = 'First') => header(title).parentElement!.querySelector('select')!
        const choose = async (type: string, title?: string) => {
            select(title).value = type
            select(title).dispatchEvent(new Event('change', { bubbles: true }))
            await tick()
        }
        for (const type of ['jailbreak', 'cot', 'plain']) {
            await choose(type)
            expect(DBState.db.promptTemplate[0]).toMatchObject({ type, text: 'Synthetic text', role: 'user' })
        }
        await choose('chat')
        await choose('jailbreak')
        expect(DBState.db.promptTemplate[0]).toMatchObject({ type: 'jailbreak', text: '', role: 'system' })

        // A block added as a type without text starts with none when it becomes a type with text.
        DBState.db.promptTemplate[1] = { name: 'Second', type: 'chat', rangeStart: -1000, rangeEnd: 'end' }
        await tick()
        toggle('Second').click()
        await tick()
        await choose('chatML', 'Second')
        expect(DBState.db.promptTemplate[1]).toMatchObject({ type: 'chatML', text: '' })
    })

    it('counts the blocks and expands or collapses all of them', async () => {
        const expanded = () => [...document.querySelectorAll('button[aria-expanded]')]
            .filter(button => !button.hasAttribute('aria-haspopup'))
            .map(button => button.getAttribute('aria-expanded'))
        expect(document.body.textContent).toContain(language.risuNest.promptList.count.replace('{0}', '3'))
        toggle('Second').click()
        await tick()
        buttonWithText(language.expandAll).click()
        await tick()
        expect(expanded()).toEqual(['true', 'true', 'true'])
        buttonWithText(language.collapseAll).click()
        await tick()
        expect(expanded()).toEqual(['false', 'false', 'false'])
    })

    it('adds a block of the chosen type, offering chain of thought only when it is enabled', async () => {
        const add = () => document.querySelector<HTMLButtonElement>('button[aria-haspopup="menu"]')!
        add().click()
        await tick()
        expect(menuItems()).toContain(language.Chat)
        expect(menuItems()).not.toContain(language.cot)
        DBState.db.promptSettings.customChainOfThought = true
        await tick()
        expect(menuItems()).toContain(language.cot)
        buttonWithText(language.Chat).click()
        await tick()
        expect(DBState.db.promptTemplate.at(-1)).toEqual({ type: 'chat', rangeStart: -1000, rangeEnd: 'end' })
        expect(menuItems()).toEqual([])

        // Escape closes only the menu, not the settings window layer it opened in.
        const settingsLayer = document.createElement('div')
        document.body.append(settingsLayer)
        const closeSettings = vi.fn()
        const layer = modalNavigation(settingsLayer, { close: closeSettings })
        add().click()
        await tick()
        document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
        await tick()
        layer.destroy()
        expect(menuItems()).toEqual([])
        expect(closeSettings).not.toHaveBeenCalled()
    })

    it('shows the role only for blocks that choose one, and the type before the text of a named block', async () => {
        DBState.db.promptTemplate = [
            { name: 'Main', type: 'plain', text: 'Line one\nline two', role: 'user', type2: 'normal' },
            { type: 'lorebook' },
            { type: 'description', innerFormat: '[Info] {{slot}}' },
        ] as typeof DBState.db.promptTemplate
        await tick()
        const summary = (title: string) => [...toggle(title).querySelectorAll('span')].map(span => span.textContent)
        expect(summary('Main')).toEqual([`Main${language.user}`, 'Main', language.user, `${language.formating.plain} · Line one line two`])
        expect(summary(language.formating.lorebook)).toEqual([language.formating.lorebook, language.formating.lorebook])
        expect(summary(language.formating.description)).toEqual([
            `${language.formating.description}${language.systemPrompt}`, language.formating.description, language.systemPrompt, '[Info] {{slot}}',
        ])
    })
})

describe('custom toggles', () => {
    it('edits the preset custom toggles through the toggle editor', async () => {
        DBState.db.customPromptTemplateToggle = 'a=A'
        const settingsTab = [...document.querySelectorAll('button')].find(button => button.textContent?.trim() === language.settings)!
        settingsTab.click()
        await tick()
        expect(mocks.toggleEditor.props.value).toBe('a=A')
        mocks.toggleEditor.props.onChange!('b=B\n=G=group')
        expect(DBState.db.customPromptTemplateToggle).toBe('b=B\n=G=group')
    })
})
