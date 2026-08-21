import { describe, expect, it, vi } from 'vitest'
import moduleSettingsSource from './ModuleSettings.svelte?raw'
import { commitModuleCharacterConversion } from './moduleCharacterConversion'

describe('module character conversion persistence', () => {
    it('awaits the named detached addition before reporting success', () => {
        expect(moduleSettingsSource).toContain('await commitModuleCharacterConversion(char')
        expect(moduleSettingsSource).toContain('commit: commitDetachedCharacter')
        expect(moduleSettingsSource).not.toContain('DBState.db.characters.push(char)')
    })

    it('reports success only after durable addition', async () => {
        const events: string[] = []

        await commitModuleCharacterConversion({ chaId: 'converted' } as any, {
            commit: vi.fn(async () => { events.push('commit') }),
            onSuccess: () => { events.push('success') },
            onError: vi.fn(),
        })

        expect(events).toEqual(['commit', 'success'])
    })

    it.each(['local', 'official'])('settles a %s rejection with one error alert', async (kind) => {
        const failure = new Error(`${kind} failed`)
        const onSuccess = vi.fn()
        const onError = vi.fn()
        const commit = vi.fn(async () => { throw failure })

        await expect(commitModuleCharacterConversion({ chaId: 'converted' } as any, {
            commit,
            onSuccess,
            onError,
        })).resolves.toBeUndefined()

        expect(commit).toHaveBeenCalledOnce()
        expect(onError).toHaveBeenCalledOnce()
        expect(onError).toHaveBeenCalledWith(failure)
        expect(onSuccess).not.toHaveBeenCalled()
    })
})
