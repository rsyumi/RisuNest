import { expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
const state = vi.hoisted(() => ({ reject: null as ((error: unknown) => void) | null }))
vi.mock('src/ts/parser/parser.svelte', () => ({
    ParseMarkdown: vi.fn(
        () =>
            new Promise((_resolve, reject) => {
                state.reject = reject
            }),
    ),
}))
vi.mock('src/ts/process/files/inlayRenderSource', () => ({
    DeferredInlayMarkerRegistry: class {
        clear() {}
    },
    mountDeferredInlaySources: () => () => {},
}))
import DeferredMarkdown from './DeferredMarkdown.svelte'

test('settles an aborted render while mounted without reporting a render error', async () => {
    const controller = new AbortController()
    const error = vi.spyOn(console, 'error').mockImplementation(() => {})
    const target = document.createElement('div')
    document.body.append(target)
    const component = mount(DeferredMarkdown, {
        target,
        props: { data: 'synthetic', signal: controller.signal },
    })
    try {
        await vi.waitFor(() => expect(state.reject).toBeTypeOf('function'))
        controller.abort()
        state.reject!(controller.signal.reason)
        await tick()
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(target.querySelector('[role="alert"]')).toBeNull()
        expect(error).not.toHaveBeenCalled()
    } finally {
        await unmount(component)
        target.remove()
        error.mockRestore()
    }
})
