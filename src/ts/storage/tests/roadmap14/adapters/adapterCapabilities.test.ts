import { describe, expect, it } from 'vitest'
import {
    ADAPTER_CAPABILITY_MATRIX,
    adapterOracleResult,
    expectedAdapterWarnings,
} from './adapterCapabilities'

describe('Roadmap 14 adapter capability oracle', () => {
    it('contains one versioned result row for every required current and future adapter', () => {
        expect(ADAPTER_CAPABILITY_MATRIX.version).toBe(1)
        expect(ADAPTER_CAPABILITY_MATRIX.rows.map((row) => row.id)).toEqual([
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
        ])
    })

    it('derives exact warnings for every non-preserved capability', () => {
        expect(expectedAdapterWarnings('risusave-block')).toEqual([
            'RisuSave stores database references only. Ordinary asset bytes remain external.',
            'RisuSave stores database references only. Inlay payload bytes remain external.',
            'RisuSave stores database references only. Cold payload bytes remain external.',
        ])
        expect(expectedAdapterWarnings('lossless-package-v1')).toEqual([
            'The future lossless package adapter is not implemented.',
        ])
    })

    it('reports unsafe current adapters and the future adapter explicitly', () => {
        expect(adapterOracleResult('risusave-block')).toEqual({
            status: 'known-gap',
            warning: 'Block RisuSave import has known required-block validation gaps.',
        })
        expect(adapterOracleResult('local-full-backup')).toEqual({
            status: 'known-gap',
            warning: 'Local backup restore has known truncation and pre-validation payload-write gaps.',
        })
        expect(adapterOracleResult('lossless-package-v1')).toEqual({
            status: 'unsupported',
            warning: 'The future lossless package adapter is not implemented.',
        })
    })

    it('has a literal warning for every intentional omission', () => {
        for (const row of ADAPTER_CAPABILITY_MATRIX.rows) {
            for (const capability of row.capabilities) {
                if (capability.category !== 'preserved') {
                    expect(capability.warning, `${row.id}:${capability.feature}`).toEqual(expect.any(String))
                    expect(capability.warning.length, `${row.id}:${capability.feature}`).toBeGreaterThan(0)
                }
            }
        }
    })
})
