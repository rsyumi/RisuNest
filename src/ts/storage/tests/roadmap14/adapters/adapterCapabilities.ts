export type AdapterId =
    | 'risusave-raw'
    | 'risusave-compressed'
    | 'risusave-stream'
    | 'risusave-block'
    | 'local-full-backup'
    | 'local-partial-backup'
    | 'drive-snapshot'
    | 'official-snapshot'
    | 'kei-backup'
    | 'card-json'
    | 'card-png'
    | 'card-charx'
    | 'card-charx-jpeg'
    | 'module-risum'
    | 'risu-sharing'
    | 'lossless-package-v1'

export type CapabilityCategory = 'preserved' | 'unsupported' | 'external'
export type AdapterOracleStatus = 'passing' | 'known-gap' | 'unsupported'

export type AdapterCapability = {
    feature: string
    category: CapabilityCategory
    warning?: string
}

export type AdapterCapabilityRow = {
    id: AdapterId
    oracleStatus: AdapterOracleStatus
    resultWarning?: string
    capabilities: readonly AdapterCapability[]
}

const DATABASE_ONLY_CAPABILITIES = [
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
] as const satisfies readonly AdapterCapability[]

const COMPLETE_BACKUP_CAPABILITIES = [
    { feature: 'database', category: 'preserved' },
    { feature: 'ordinary-assets', category: 'preserved' },
    { feature: 'inlays', category: 'preserved' },
    { feature: 'cold-payloads', category: 'preserved' },
] as const satisfies readonly AdapterCapability[]

const CHARX_CAPABILITIES = [
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
] as const satisfies readonly AdapterCapability[]

export const ADAPTER_CAPABILITY_MATRIX = {
    version: 1,
    rows: [
        { id: 'risusave-raw', oracleStatus: 'passing', capabilities: DATABASE_ONLY_CAPABILITIES },
        { id: 'risusave-compressed', oracleStatus: 'passing', capabilities: DATABASE_ONLY_CAPABILITIES },
        { id: 'risusave-stream', oracleStatus: 'passing', capabilities: DATABASE_ONLY_CAPABILITIES },
        {
            id: 'risusave-block',
            oracleStatus: 'known-gap',
            resultWarning: 'Block RisuSave import has known required-block validation gaps.',
            capabilities: DATABASE_ONLY_CAPABILITIES,
        },
        {
            id: 'local-full-backup',
            oracleStatus: 'known-gap',
            resultWarning: 'Local backup restore has known truncation and pre-validation payload-write gaps.',
            capabilities: COMPLETE_BACKUP_CAPABILITIES,
        },
        {
            id: 'local-partial-backup',
            oracleStatus: 'known-gap',
            resultWarning: 'Partial local backup restore shares the known local restore atomicity gaps.',
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
            oracleStatus: 'passing',
            capabilities: COMPLETE_BACKUP_CAPABILITIES,
        },
        {
            id: 'official-snapshot',
            oracleStatus: 'passing',
            capabilities: COMPLETE_BACKUP_CAPABILITIES,
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
            resultWarning: 'CharX has known sanitized and case-folded path collision gaps.',
            capabilities: CHARX_CAPABILITIES,
        },
        {
            id: 'card-charx-jpeg',
            oracleStatus: 'known-gap',
            resultWarning: 'CharX-JPEG has known sanitized and case-folded path collision gaps.',
            capabilities: CHARX_CAPABILITIES,
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
            oracleStatus: 'unsupported',
            resultWarning: 'The future lossless package adapter is not implemented.',
            capabilities: [
                {
                    feature: 'adapter',
                    category: 'unsupported',
                    warning: 'The future lossless package adapter is not implemented.',
                },
            ],
        },
    ],
} as const satisfies {
    version: number
    rows: readonly AdapterCapabilityRow[]
}

export function expectedAdapterWarnings(id: AdapterId): string[] {
    const row = ADAPTER_CAPABILITY_MATRIX.rows.find((candidate) => candidate.id === id)
    if (!row) throw new Error(`Unknown adapter capability row: ${id}`)
    return row.capabilities.flatMap((capability) =>
        capability.category === 'preserved' ? [] : [capability.warning],
    )
}

export function adapterOracleResult(id: AdapterId):
    | { status: 'passing' }
    | { status: 'known-gap' | 'unsupported'; warning: string } {
    const row = ADAPTER_CAPABILITY_MATRIX.rows.find((candidate) => candidate.id === id)
    if (!row) throw new Error(`Unknown adapter capability row: ${id}`)
    if (row.oracleStatus === 'passing') return { status: 'passing' }
    return { status: row.oracleStatus, warning: row.resultWarning }
}
