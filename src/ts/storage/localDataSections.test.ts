import { beforeEach, describe, expect, it, vi } from 'vitest'

const core = vi.hoisted(() => ({ invoke: vi.fn() }))

vi.mock('@tauri-apps/api/core', () => core)

import { readLocalDataParticipation, setLocalDataParticipating } from './localDataSections'

beforeEach(() => {
    core.invoke.mockReset()
})

describe('localDataSections', () => {
    it('reads the shipped sections through pds_read_section_participation', async () => {
        core.invoke.mockResolvedValue([
            { section: 'hypa', participating: true },
            { section: 'local-plugins', participating: false },
        ])
        await expect(readLocalDataParticipation()).resolves.toEqual([
            { section: 'hypa', participating: true },
            { section: 'local-plugins', participating: false },
        ])
        expect(core.invoke).toHaveBeenCalledWith('pds_read_section_participation')
    })

    it('writes one section through pds_set_section_participating', async () => {
        core.invoke.mockResolvedValue(undefined)
        await setLocalDataParticipating('local-plugins', true)
        expect(core.invoke).toHaveBeenCalledWith('pds_set_section_participating', {
            section: 'local-plugins',
            participating: true,
        })
    })
})
