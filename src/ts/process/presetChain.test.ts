import { describe, expect, it, vi } from 'vitest'
import { activatePresetChain, activatePresetChainForRequest } from './presetChain'

describe('preset chain activation', () => {
    it('does not continue request construction before the selected preset is active', async () => {
        const events: string[] = []
        let finish!: () => void
        const changePreset = vi.fn(() => new Promise<void>((resolve) => {
            finish = () => {
                events.push('active')
                resolve()
            }
        }))
        const run = async () => {
            await activatePresetChain(
                { presetChain: 'Target', botPresets: [{ name: 'Target' }] },
                changePreset,
                () => 0,
                vi.fn(),
            )
            events.push('request')
        }

        const pending = run()
        await Promise.resolve()

        expect(changePreset).toHaveBeenCalledWith(0, true)
        expect(events).toEqual([])

        finish()
        await pending

        expect(events).toEqual(['active', 'request'])
    })

    it('reports a missing configured preset without activating another one', async () => {
        const changePreset = vi.fn()
        const onMissing = vi.fn()

        await activatePresetChain(
            { presetChain: 'Missing', botPresets: [{ name: 'Other' }] },
            changePreset,
            () => 0,
            onMissing,
        )

        expect(changePreset).not.toHaveBeenCalled()
        expect(onMissing).toHaveBeenCalledWith('Missing')
    })

    it('never starts the request when preset activation fails', async () => {
        const events: string[] = []
        const changePreset = vi.fn(async () => {
            throw new Error('preset CAS failed')
        })
        const run = async () => {
            await activatePresetChainForRequest(
                { presetChain: 'Target', botPresets: [{ name: 'Target' }] },
                changePreset,
                () => 0,
                vi.fn(),
            )
            events.push('request')
        }

        await expect(run()).rejects.toThrow('preset CAS failed')

        expect(events).not.toContain('request')
    })
})
