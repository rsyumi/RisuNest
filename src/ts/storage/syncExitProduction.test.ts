import { describe, expect, it, vi } from 'vitest'
import { registerWindowCloseDrain } from './syncExitProduction'

function harness(result: 'exit' | 'cancelled' = 'exit') {
    let handler!: (event: { preventDefault(): void }) => Promise<void>
    const unlisten = vi.fn()
    const window = {
        onCloseRequested: vi.fn(async (next: typeof handler) => {
            handler = next
            return unlisten
        }),
        close: vi.fn(async () => {}),
    }
    const coordinator = {
        requestExit: vi.fn(async () => result),
    }
    return { window, coordinator, unlisten, getHandler: () => handler }
}

describe('window close exit drain', () => {
    it('coalesces duplicate native close events while the first drain is pending', async () => {
        let finish!: (result: 'exit') => void
        const h = harness()
        h.coordinator.requestExit.mockImplementationOnce(
            () => new Promise<'exit'>((resolve) => { finish = resolve }),
        )
        await registerWindowCloseDrain(h.window, h.coordinator as never)

        const first = h.getHandler()({ preventDefault: vi.fn() })
        const second = h.getHandler()({ preventDefault: vi.fn() })
        expect(h.coordinator.requestExit).toHaveBeenCalledOnce()
        finish('exit')
        await Promise.all([first, second])

        expect(h.window.close).toHaveBeenCalledOnce()
    })

    it('holds a close request until the coordinator permits one recursive close', async () => {
        const h = harness()
        await registerWindowCloseDrain(h.window, h.coordinator as never)
        const preventDefault = vi.fn()

        await h.getHandler()({ preventDefault })

        expect(preventDefault).toHaveBeenCalledOnce()
        expect(h.coordinator.requestExit).toHaveBeenCalledOnce()
        expect(h.window.close).toHaveBeenCalledOnce()

        const recursivePrevent = vi.fn()
        await h.getHandler()({ preventDefault: recursivePrevent })
        expect(recursivePrevent).not.toHaveBeenCalled()
        expect(h.window.close).toHaveBeenCalledOnce()
    })

    it('keeps the window open when exit is cancelled', async () => {
        const h = harness('cancelled')
        await registerWindowCloseDrain(h.window, h.coordinator as never)
        const preventDefault = vi.fn()

        await h.getHandler()({ preventDefault })

        expect(preventDefault).toHaveBeenCalledOnce()
        expect(h.window.close).not.toHaveBeenCalled()
    })
})
