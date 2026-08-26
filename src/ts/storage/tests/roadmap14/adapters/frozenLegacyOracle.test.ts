import { describe, expect, it } from 'vitest'
import { vi } from 'vitest'
import { decodeRisuSave } from '../../../risuSave'
import {
    verifyFrozenCanonicalText,
    verifyFrozenLegacyArtifacts,
} from './frozenLegacyOracle'

vi.mock('../../../database.svelte', () => ({
    presetTemplate: {},
}))
vi.mock('../../../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

describe('frozen legacy reverse-import oracle', () => {
    it('normalizes only CRLF checkout text before frozen hash and exact comparison', () => {
        const canonicalLf = '{\n  "value": 1\n}\n'
        const checkoutCrlf = canonicalLf.replaceAll('\n', '\r\n')
        const pinnedLfSha256 = '6bb1b9dd0e4bc676b1627469d12c2187c08145f30f557ebed8cfc3415f7d4f9c'

        expect(() => verifyFrozenCanonicalText({
            artifactId: 'newline-regression',
            checkoutText: checkoutCrlf,
            pinnedLfSha256,
            actualCanonical: canonicalLf,
        })).not.toThrow()

        expect(() => verifyFrozenCanonicalText({
            artifactId: 'newline-regression',
            checkoutText: checkoutCrlf.replace('1', '2'),
            pinnedLfSha256,
            actualCanonical: canonicalLf,
        })).toThrow('Frozen legacy canonical artifact changed: newline-regression')

        expect(() => verifyFrozenCanonicalText({
            artifactId: 'newline-regression',
            checkoutText: checkoutCrlf,
            pinnedLfSha256,
            actualCanonical: canonicalLf.replace('1', '2'),
        })).toThrow('Legacy reverse import changed: newline-regression')
    })

    it('decodes the checked-in legacy input to the checked-in canonical output', async () => {
        await expect(verifyFrozenLegacyArtifacts(decodeRisuSave)).resolves.toEqual([
            { artifactId: 'risusave-raw-v4-2026-08-26', status: 'passing', warnings: [] },
            { artifactId: 'risusave-compressed-v4-2026-08-26', status: 'passing', warnings: [] },
            { artifactId: 'risusave-stream-v4-2026-08-26', status: 'passing', warnings: [] },
            { artifactId: 'risusave-block-v4-2026-08-26', status: 'passing', warnings: [] },
        ])
    })
})
