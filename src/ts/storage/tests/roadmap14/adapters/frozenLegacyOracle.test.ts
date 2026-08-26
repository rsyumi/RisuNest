import { describe, expect, it } from 'vitest'
import { vi } from 'vitest'
import { decodeRisuSave } from '../../../risuSave'
import { verifyFrozenLegacyArtifacts } from './frozenLegacyOracle'

vi.mock('../../../database.svelte', () => ({
    presetTemplate: {},
}))
vi.mock('../../../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

describe('frozen legacy reverse-import oracle', () => {
    it('decodes the checked-in legacy input to the checked-in canonical output', async () => {
        await expect(verifyFrozenLegacyArtifacts(decodeRisuSave)).resolves.toEqual([
            { artifactId: 'risusave-raw-v4-2026-08-26', status: 'passing', warnings: [] },
            { artifactId: 'risusave-compressed-v4-2026-08-26', status: 'passing', warnings: [] },
            { artifactId: 'risusave-stream-v4-2026-08-26', status: 'passing', warnings: [] },
            { artifactId: 'risusave-block-v4-2026-08-26', status: 'passing', warnings: [] },
        ])
    })
})
