import { describe, expect, it } from 'vitest'
import { ADAPTER_CAPABILITY_MATRIX } from './adapterCapabilities'

const EXPECTED_ADAPTER_CAPABILITY_MATRIX = {
    version: 1,
    rows: [
        {
            id: 'risusave-raw',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Ordinary asset bytes remain external.',
                },
                {
                    feature: 'inlays',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Inlay payload bytes remain external.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Cold payload bytes remain external.',
                },
            ],
        },
        {
            id: 'risusave-compressed',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Ordinary asset bytes remain external.',
                },
                {
                    feature: 'inlays',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Inlay payload bytes remain external.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Cold payload bytes remain external.',
                },
            ],
        },
        {
            id: 'risusave-stream',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Ordinary asset bytes remain external.',
                },
                {
                    feature: 'inlays',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Inlay payload bytes remain external.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Cold payload bytes remain external.',
                },
            ],
        },
        {
            id: 'risusave-block',
            oracleStatus: 'known-gap',
            resultWarning: 'Block RisuSave import has known required-block validation gaps.',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Ordinary asset bytes remain external.',
                },
                {
                    feature: 'inlays',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Inlay payload bytes remain external.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'external',
                    warning: 'RisuSave stores database references only. Cold payload bytes remain external.',
                },
            ],
        },
        {
            id: 'local-full-backup',
            oracleStatus: 'known-gap',
            resultWarning: 'Local backup restore safety is fixture-only and unprobed at the production boundary.',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                { feature: 'ordinary-assets', category: 'preserved' },
                { feature: 'inlays', category: 'preserved' },
                { feature: 'cold-payloads', category: 'preserved' },
            ],
        },
        {
            id: 'local-partial-backup',
            oracleStatus: 'known-gap',
            resultWarning: 'Partial local restore safety is fixture-only and unprobed at the production boundary.',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'unsupported',
                    warning: 'Partial local backup omits assets outside its documented profile selection.',
                },
                {
                    feature: 'inlays',
                    category: 'unsupported',
                    warning: 'Partial local backup omits Inlays outside its documented profile selection.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'unsupported',
                    warning: 'Partial local backup may omit cold payloads outside its documented selection.',
                },
            ],
        },
        {
            id: 'drive-snapshot',
            oracleStatus: 'known-gap',
            resultWarning: 'Drive snapshot does not transfer referenced Inlay payload bytes.',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                { feature: 'ordinary-assets', category: 'preserved' },
                {
                    feature: 'inlays',
                    category: 'unsupported',
                    warning: 'Drive snapshot does not upload or restore referenced Inlay payload bytes.',
                },
                { feature: 'cold-payloads', category: 'preserved' },
            ],
        },
        {
            id: 'official-snapshot',
            oracleStatus: 'known-gap',
            resultWarning: 'Official snapshot does not transfer referenced Inlay payload bytes.',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                { feature: 'ordinary-assets', category: 'preserved' },
                {
                    feature: 'inlays',
                    category: 'unsupported',
                    warning: 'Official snapshot does not publish or restore referenced Inlay payload bytes.',
                },
                { feature: 'cold-payloads', category: 'preserved' },
            ],
        },
        {
            id: 'kei-backup',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'external',
                    warning: 'KEI backup sends database JSON. Ordinary asset bytes remain external.',
                },
                {
                    feature: 'inlays',
                    category: 'external',
                    warning: 'KEI backup sends database JSON. Inlay payload bytes remain external.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'external',
                    warning: 'KEI backup sends database JSON. Cold payload bytes remain external.',
                },
            ],
        },
        {
            id: 'card-json',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'character-data', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'unsupported',
                    warning: 'JSON character cards do not embed referenced asset bytes.',
                },
            ],
        },
        {
            id: 'card-png',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'character-data', category: 'preserved' },
                { feature: 'card-image', category: 'preserved' },
                {
                    feature: 'ordinary-assets',
                    category: 'unsupported',
                    warning: 'PNG character cards do not embed arbitrary referenced asset bytes.',
                },
            ],
        },
        {
            id: 'card-charx',
            oracleStatus: 'known-gap',
            resultWarning: 'CharX collision safety is fixture-only and unprobed at the production boundary.',
            capabilities: [
                { feature: 'character-data', category: 'preserved' },
                { feature: 'ordinary-assets', category: 'preserved' },
                {
                    feature: 'inlays',
                    category: 'external',
                    warning: 'CharX does not embed application Inlay payload storage.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'external',
                    warning: 'CharX does not embed application cold payload storage.',
                },
            ],
        },
        {
            id: 'card-charx-jpeg',
            oracleStatus: 'known-gap',
            resultWarning: 'CharX-JPEG collision safety is fixture-only and unprobed at the production boundary.',
            capabilities: [
                { feature: 'character-data', category: 'preserved' },
                { feature: 'ordinary-assets', category: 'preserved' },
                {
                    feature: 'inlays',
                    category: 'external',
                    warning: 'CharX does not embed application Inlay payload storage.',
                },
                {
                    feature: 'cold-payloads',
                    category: 'external',
                    warning: 'CharX does not embed application cold payload storage.',
                },
            ],
        },
        {
            id: 'module-risum',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'module-data', category: 'preserved' },
                { feature: 'ordinary-assets', category: 'preserved' },
                {
                    feature: 'database',
                    category: 'unsupported',
                    warning: 'Risu module containers carry one module, not a complete database.',
                },
            ],
        },
        {
            id: 'risu-sharing',
            oracleStatus: 'passing',
            capabilities: [
                { feature: 'shared-record-data', category: 'preserved' },
                { feature: 'referenced-assets', category: 'preserved' },
                {
                    feature: 'database',
                    category: 'unsupported',
                    warning: 'Risu sharing containers carry selected records, not a complete database.',
                },
            ],
        },
        {
            id: 'lossless-package-v1',
            oracleStatus: 'known-gap',
            resultWarning: 'The private native foundation preserves all package data, but production routing remains disabled pending physical atomicity gates.',
            capabilities: [
                { feature: 'database', category: 'preserved' },
                { feature: 'ordinary-assets', category: 'preserved' },
                { feature: 'inlays', category: 'preserved' },
                { feature: 'cold-payloads', category: 'preserved' },
                {
                    feature: 'production-route',
                    category: 'partial',
                    warning: 'The public route stays disabled until 10 GB, process-kill, disk-full, and Android evidence passes.',
                },
            ],
        },
    ],
}

describe('Roadmap 14 adapter capability oracle', () => {
    it('matches the complete independent capability, result, and warning table', () => {
        expect(ADAPTER_CAPABILITY_MATRIX).toEqual(EXPECTED_ADAPTER_CAPABILITY_MATRIX)
    })
})
