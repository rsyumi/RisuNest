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
        destroy: vi.fn(async () => {}),
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

        expect(h.window.destroy).toHaveBeenCalledOnce()
    })

    it('holds a close request until the coordinator permits one direct destroy', async () => {
        const h = harness()
        await registerWindowCloseDrain(h.window, h.coordinator as never)
        const preventDefault = vi.fn()

        await h.getHandler()({ preventDefault })

        expect(preventDefault).toHaveBeenCalledOnce()
        expect(h.coordinator.requestExit).toHaveBeenCalledOnce()
        expect(h.window.destroy).toHaveBeenCalledOnce()

        const subsequentPrevent = vi.fn()
        await h.getHandler()({ preventDefault: subsequentPrevent })
        expect(subsequentPrevent).not.toHaveBeenCalled()
        expect(h.window.destroy).toHaveBeenCalledOnce()
    })

    it('keeps the window open when exit is cancelled', async () => {
        const h = harness('cancelled')
        await registerWindowCloseDrain(h.window, h.coordinator as never)
        const preventDefault = vi.fn()

        await h.getHandler()({ preventDefault })

        expect(preventDefault).toHaveBeenCalledOnce()
        expect(h.window.destroy).not.toHaveBeenCalled()
    })

    it('allows another close request when the native destroy command fails', async () => {
        const h = harness()
        h.window.destroy.mockRejectedValueOnce(new Error('destroy denied'))
        const reportError = vi.fn()
        await registerWindowCloseDrain(h.window, h.coordinator as never, reportError)

        await h.getHandler()({ preventDefault: vi.fn() })
        expect(reportError).toHaveBeenCalledOnce()
        await h.getHandler()({ preventDefault: vi.fn() })

        expect(h.coordinator.requestExit).toHaveBeenCalledTimes(2)
        expect(h.window.destroy).toHaveBeenCalledTimes(2)
    })
})
