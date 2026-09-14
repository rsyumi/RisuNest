import { describe, expect, it } from 'vitest'
import { ADAPTER_CAPABILITY_MATRIX, type AdapterId } from './adapterCapabilities'

const EXPECTED_ADAPTER_IDS = [
    'risusave-raw',
    'risusave-compressed',
    'risusave-stream',
    'risusave-block',
    'local-full-backup',
    'local-partial-backup',
    'drive-snapshot',
    'official-snapshot',
    'kei-backup',
    'card-json',
    'card-png',
    'card-charx',
    'card-charx-jpeg',
    'module-risum',
    'risu-sharing',
    'lossless-package-v1',
] as const satisfies readonly AdapterId[]

describe('Roadmap 14 adapter capability manifest', () => {
    it('contains every declared adapter exactly once', () => {
        const ids = ADAPTER_CAPABILITY_MATRIX.rows.map((row) => row.id)
        expect(ids).toEqual(EXPECTED_ADAPTER_IDS)
        expect(new Set(ids).size).toBe(ids.length)
    })

    it('requires actionable warnings for every non-passing row and partial capability', () => {
        for (const row of ADAPTER_CAPABILITY_MATRIX.rows) {
            if (row.oracleStatus === 'passing') {
                expect(row).not.toHaveProperty('resultWarning')
            } else {
                expect(row.resultWarning?.trim()).toBeTruthy()
            }
            for (const capability of row.capabilities) {
                if (capability.category === 'preserved') continue
                expect(capability.warning?.trim()).toBeTruthy()
            }
        }
    })

    it('records native CharX collision handling as covered for both containers', () => {
        for (const id of ['card-charx', 'card-charx-jpeg'] as const) {
            expect(ADAPTER_CAPABILITY_MATRIX.rows.find((row) => row.id === id))
                .toMatchObject({ id, oracleStatus: 'passing' })
        }
    })
})
