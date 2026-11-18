import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { language } from 'src/lang'
import ToggleDefinitionEditor from './ToggleDefinitionEditor.svelte'

const textAreas = vi.hoisted(() => [] as { value: string; optimaizedInput?: boolean; onInput?: () => void }[])

vi.mock('src/ts/storage/database.svelte', () => ({}))
vi.mock('src/ts/characters', () => ({}))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: {} }, selectedCharID: writable(0) }))
vi.mock('src/ts/process/modules', () => ({ getModuleToggles: () => '' }))
vi.mock('src/lib/UI/PopupList.svelte', () => ({ default: {} }))
vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))
// The text area exposes the props it receives, so a raw edit can go through its value binding.
vi.mock('src/lib/UI/GUI/TextAreaInput.svelte', () => ({
    default: (_anchor: unknown, props: (typeof textAreas)[number]) => { textAreas.push(props) },
}))

let instance: ReturnType<typeof mount> | undefined
let props: { value: unknown; onChange: ReturnType<typeof vi.fn<(value: string) => void>> }

async function open(value: unknown) {
    const next = $state({ value, onChange: vi.fn<(value: string) => void>() })
    props = next
    instance = mount(ToggleDefinitionEditor, { target: document.body, props })
    await tick()
}
const all = <T extends Element>(selector: string) => [...document.querySelectorAll<T>(selector)]
const names = () => all<HTMLInputElement>(`input[aria-label="${language.name}"]`)
const keys = () => all<HTMLInputElement>(`input[aria-label="${language.key}"]`)
const types = () => all<HTMLSelectElement>(`select[aria-label="${language.type}"]`)
const buttons = (label: string) => all<HTMLButtonElement>(`button[aria-label="${label}"]`)
const button = (text: string) => all<HTMLButtonElement>('button').find((element) => element.textContent?.trim() === text)!
async function type(input: HTMLInputElement, value: string) {
    input.value = value
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await tick()
}
function preview() {
    return all<HTMLElement>('div').find((element) => element.textContent === language.preview)?.parentElement ?? null
}

afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    textAreas.length = 0
    document.body.replaceChildren()
})

describe('toggle definition editor', () => {
    it('writes nothing when it opens or switches views', async () => {
        const texts = [undefined, 7, '', 'a=A\r\n\n=G=group\r\nb=B\n  junk\n', '=Open=group\nx=X=select=1,2', '\n\n']
        for (const value of texts) {
            await open(value)
            button(language.risuNest.toggleEditor.raw).click()
            await tick()
            button(language.hotkeyDesc.edit).click()
            await tick()
            expect(props.onChange, JSON.stringify(value)).not.toHaveBeenCalled()
            await unmount(instance!)
            instance = undefined
        }
    })

    it('changes only the edited line and keeps its inputs when the saved text comes back', async () => {
        const text = 'a=A\r\n\n=G=group\nb=B\n=G=groupEnd\n  junk line\r\nc=C=select=x,y\r\n'
        await open(text)
        expect(names().map((input) => input.value)).toEqual(['A', 'G', 'B', 'C'])
        const name = names()[2]
        await type(name, 'Bee')
        const saved = 'a=A\r\n\n=G=group\nb=Bee\n=G=groupEnd\n  junk line\r\nc=C=select=x,y\r\n'
        expect(props.onChange).toHaveBeenLastCalledWith(saved)
        props.value = saved
        await tick()
        expect(names()[2]).toBe(name)

        const key = keys()[0]
        await type(key, 'a=')
        expect(key.value).toBe('a')
        expect(props.onChange).toHaveBeenCalledTimes(1)
        await type(key, 'k=ey')
        expect(key.value).toBe('key')
        expect(props.onChange).toHaveBeenLastCalledWith(saved.replace('a=A', 'key=A'))
    })

    it('reads a text that changes elsewhere again', async () => {
        await open('a=A')
        props.value = 'b=B\nc=C'
        await tick()
        expect(names().map((input) => input.value)).toEqual(['B', 'C'])
        expect(props.onChange).not.toHaveBeenCalled()
    })

    it('shows each row type, changes it, and offers groups only outside a group', async () => {
        const text = '=G=group\nc=C=select=x\n=G=groupEnd\nd=D=textarea\n==divider\n=Cap=caption\ne=E=text\r\nf=F'
        await open(text)
        expect(types().map((select) => select.value)).toEqual(['group', 'select', 'textarea', 'divider', 'caption', 'switch', 'switch'])
        const offered = (select: HTMLSelectElement) => [...select.options].map((option) => option.value)
        expect(offered(types()[0])).toContain('group')
        expect(offered(types()[1])).not.toContain('group')
        const select = types()[6]
        select.value = 'group'
        select.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith(text.replace('f=F', '=F=group\n=F=groupEnd'))
    })

    it('moves, removes and adds rows', async () => {
        await open('a=A\nb=B\nc=C\n')
        expect(buttons(language.movePresetUp).map((element) => element.disabled)).toEqual([true, false, false])
        expect(buttons(language.movePresetDown).map((element) => element.disabled)).toEqual([false, false, true])
        buttons(language.movePresetDown)[0].click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('b=B\na=A\nc=C\n')
        buttons(language.remove)[2].click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('b=B\na=A\n')
        button(language.add).click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('b=B\na=A\n=\n')
        expect(names().map((input) => input.value)).toEqual(['B', 'A', ''])
    })

    it('shows the same text in the raw view and saves raw edits', async () => {
        const text = 'a=A\r\n=G=group\nb=B'
        await open(text)
        button(language.risuNest.toggleEditor.raw).click()
        await tick()
        const raw = textAreas.find((area) => area.optimaizedInput === false)!
        expect(raw.value).toBe(text)
        raw.onInput!()
        expect(props.onChange).not.toHaveBeenCalled()
        raw.value = 'x=X'
        raw.onInput!()
        expect(props.onChange).toHaveBeenLastCalledWith('x=X')
        button(language.hotkeyDesc.edit).click()
        await tick()
        expect(names().map((input) => input.value)).toEqual(['X'])
    })

    it('previews the edited rows without saving preview values', async () => {
        await open('p=Power\nq=Quiet')
        await type(names()[0], 'Boost')
        expect(preview()?.textContent).toContain('Boost')
        props.onChange.mockClear()
        const power = preview()!.querySelector<HTMLInputElement>('input[role="switch"]')!
        power.click()
        await tick()
        expect(power.checked).toBe(true)
        expect(props.onChange).not.toHaveBeenCalled()
    })
})
