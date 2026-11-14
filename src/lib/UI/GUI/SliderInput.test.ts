import { afterEach, expect, it, vi } from 'vitest'
import { flushSync, mount, unmount } from 'svelte'
import SliderInput from './SliderInput.svelte'

let instance: ReturnType<typeof mount> | undefined

// A 200 px track from 0 to 100, so a pointer at clientX x sets x / 2.
function render(initial: number, props: { disableable?: boolean } = {}) {
    const bound = { value: initial }
    instance = mount(SliderInput, {
        target: document.body,
        props: {
            min: 0,
            max: 100,
            ...props,
            get value() { return bound.value },
            set value(next: number) { bound.value = next },
        },
    })
    flushSync()
    const track = document.querySelector<HTMLElement>('[role="slider"]')!
    vi.spyOn(track, 'getBoundingClientRect').mockReturnValue(new DOMRect(0, 0, 200, 32))
    const pointer = (type: string, pointerType: 'touch' | 'mouse', clientX: number, clientY = 16, buttons = type === 'pointerup' || type === 'pointercancel' ? 0 : 1) => {
        track.dispatchEvent(new PointerEvent(type, { bubbles: true, pointerId: pointerType === 'touch' ? 7 : 1, pointerType, clientX, clientY, buttons }))
        flushSync()
    }
    return { bound, track, pointer }
}

afterEach(() => {
    if (instance) unmount(instance)
    instance = undefined
    document.body.replaceChildren()
    vi.restoreAllMocks()
})

it('keeps the value when a finger that lands on the track scrolls the page instead', () => {
    const { bound, pointer } = render(25)
    pointer('pointerdown', 'touch', 160)
    pointer('pointermove', 'touch', 162, 40)
    pointer('pointermove', 'touch', 163, 90)
    expect(bound.value).toBe(25)
    pointer('pointercancel', 'touch', 163, 90)
    expect(bound.value).toBe(25)

    pointer('pointerdown', 'touch', 160)
    pointer('pointermove', 'touch', 160, 90)
    pointer('pointerup', 'touch', 160, 90)
    expect(bound.value).toBe(25)
})

it('follows a finger that slides along the track and sets the value where a finger taps', () => {
    const { bound, pointer } = render(25)
    pointer('pointerdown', 'touch', 40)
    expect(bound.value).toBe(25)
    pointer('pointermove', 'touch', 100, 20)
    expect(bound.value).toBe(50)
    pointer('pointermove', 'touch', 160, 30)
    expect(bound.value).toBe(80)
    pointer('pointerup', 'touch', 160, 30)
    expect(bound.value).toBe(80)

    pointer('pointerdown', 'touch', 20)
    pointer('pointerup', 'touch', 20)
    expect(bound.value).toBe(10)
})

it.each([
    { initial: 25, props: {} },
    { initial: -1000, props: { disableable: true } },
])('returns a sliding finger to $initial when the browser cancels the touch', ({ initial, props }) => {
    const { bound, pointer } = render(initial, props)
    pointer('pointerdown', 'touch', 40)
    pointer('pointermove', 'touch', 120)
    expect(bound.value).toBe(60)
    pointer('pointercancel', 'touch', 120)
    expect(bound.value).toBe(initial)
})

it('sets the value on a mouse press and keeps dragging outside the track until release', () => {
    const { bound, track, pointer } = render(25)
    pointer('pointerdown', 'mouse', 100)
    expect(bound.value).toBe(50)
    track.dispatchEvent(new PointerEvent('pointerleave', { pointerId: 1, pointerType: 'mouse', clientX: 260, buttons: 1 }))
    pointer('pointermove', 'mouse', 260, 80)
    expect(bound.value).toBe(100)
    pointer('pointerup', 'mouse', 260, 80)
    pointer('pointermove', 'mouse', 40, 16, 0)
    expect(bound.value).toBe(100)

    // A release that never reached the track ends the drag at the next move without a pressed button.
    pointer('pointerdown', 'mouse', 100)
    pointer('pointermove', 'mouse', 40, 16, 0)
    pointer('pointermove', 'mouse', 20, 16, 0)
    expect(bound.value).toBe(50)
})
