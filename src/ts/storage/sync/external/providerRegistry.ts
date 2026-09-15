import type {
    ExternalConnectionConfig,
    ExternalProviderId,
    ExternalProviderSecretInput,
    ExternalPublicationStrategy,
} from './types'

/** Field shape only; labels, help and option names come from the settings strings. */
export interface ExternalProviderField {
    key: string
    /** Example value shown as the input placeholder. */
    placeholder?: string
    required?: boolean
    secret?: boolean
    type?: 'text' | 'password' | 'select' | 'datetime-local'
    options?: string[]
    location?: boolean
}

export interface ExternalProviderDefinition {
    id: ExternalProviderId
    defaultEndpoint: string
    /** Whether the server address is user-provided. Fixed-address services hide the field. */
    customEndpoint: boolean
    /** Profile values with the service's own display names where one exists. */
    profiles: Array<{ value: string; label: string }>
    fields: ExternalProviderField[]
    secretFields: ExternalProviderField[]
    oauth: boolean
    strategies: ExternalPublicationStrategy[]
}

const s3Profiles = [
    { value: 'r2', label: 'Cloudflare R2' },
    { value: 'b2', label: 'Backblaze B2' },
    { value: 'hf', label: 'Hugging Face Storage Buckets' },
    { value: 'generic', label: '' },
]

export const externalProviderDefinitions: ExternalProviderDefinition[] = [
    {
        id: 'webdav', oauth: false, customEndpoint: true,
        defaultEndpoint: '', profiles: [{ value: '', label: '' }, { value: 'koofr', label: 'Koofr' }],
        fields: [{ key: 'accountId', required: true }, { key: 'root', required: true, location: true, placeholder: 'RisuNest' }],
        secretFields: [{ key: 'password', required: true, secret: true }],
        strategies: ['cas', 'sequential', 'backup-only'],
    },
    {
        id: 's3', oauth: false, customEndpoint: true,
        defaultEndpoint: '', profiles: s3Profiles,
        fields: [
            { key: 'accountId', required: true },
            { key: 'bucket', required: true, location: true },
            { key: 'prefix', location: true, placeholder: 'risunest' },
            { key: 'region', location: true, placeholder: 'auto' },
            { key: 'addressing', location: true, type: 'select', options: ['', 'path', 'virtual'] },
        ],
        secretFields: [{ key: 'accessKeyId', required: true, secret: true }, { key: 'secretAccessKey', required: true, secret: true }],
        strategies: ['cas', 'sequential', 'backup-only'],
    },
    {
        id: 'google_drive', oauth: true, customEndpoint: false,
        defaultEndpoint: 'https://www.googleapis.com', profiles: [{ value: 'drive', label: 'Google Drive' }],
        fields: [
            { key: 'accountId' },
            { key: 'folderId', required: true, location: true },
            { key: 'space', location: true, type: 'select', options: ['drive', 'appDataFolder'] },
            { key: 'oauthRedirectUri', required: true, location: true },
            { key: 'projectId', required: true },
            { key: 'clientId', required: true },
        ], secretFields: [], strategies: ['sequential', 'backup-only'],
    },
    {
        // No CAS: Graph has no conditional update on the content PUT used for a head.
        id: 'onedrive', oauth: true, customEndpoint: false,
        defaultEndpoint: 'https://graph.microsoft.com/v1.0', profiles: [],
        fields: [
            { key: 'accountId' },
            { key: 'accountType', required: true, location: true, type: 'select', options: ['personal', 'business', 'appFolder'] },
            { key: 'tenant', required: true, location: true, placeholder: 'common' },
            { key: 'driveId', required: true, location: true },
            { key: 'rootItemId', required: true, location: true },
            { key: 'redirectUri', required: true, location: true },
            { key: 'projectId', required: true },
            { key: 'clientId', required: true },
        ], secretFields: [], strategies: ['sequential', 'backup-only'],
    },
    {
        id: 'mybox', oauth: false, customEndpoint: false,
        defaultEndpoint: 'https://open-api.mybox.naver.com/v1',
        profiles: ['plan30gb', 'plan80gb', 'plan180gb', 'plan2tb', 'plan5tb', 'plan10tb', 'plan20tb'].map(value => ({ value, label: '' })),
        fields: [{ key: 'accountId', required: true }, { key: 'rootFolderName', required: true, location: true }, { key: 'rootFolderId', location: true }],
        secretFields: [{ key: 'pat', required: true, secret: true }, { key: 'expiresAtMs', required: true, type: 'datetime-local' }],
        strategies: ['sequential', 'backup-only'],
    },
    {
        id: 'github_releases', oauth: false, customEndpoint: false,
        defaultEndpoint: 'https://api.github.com', profiles: [],
        fields: [{ key: 'accountId', required: true }, { key: 'uploadEndpoint', required: true, location: true, placeholder: 'https://uploads.github.com' }, { key: 'owner', required: true, location: true }, { key: 'repo', required: true, location: true }, { key: 'tagPrefix', required: true, location: true, placeholder: 'risunest-backup' }],
        secretFields: [{ key: 'token', required: true, secret: true }],
        strategies: ['backup-only'],
    },
    {
        id: 'gitlab_packages', oauth: false, customEndpoint: true,
        defaultEndpoint: 'https://gitlab.com', profiles: [{ value: '', label: '' }, { value: 'gitlabCom', label: 'GitLab.com' }, { value: 'selfManaged', label: '' }],
        fields: [{ key: 'accountId', required: true }, { key: 'projectId', required: true, location: true }, { key: 'packageName', required: true, location: true, placeholder: 'risunest-backup' }, { key: 'maxFileBytes', location: true }],
        secretFields: [{ key: 'token', required: true, secret: true }, { key: 'tokenKind', required: true, type: 'select', options: ['personalAccessToken', 'projectAccessToken', 'deployToken'] }],
        strategies: ['backup-only'],
    },
]

export function getExternalProviderDefinition(id: ExternalProviderId): ExternalProviderDefinition {
    const definition = externalProviderDefinitions.find(provider => provider.id === id)
    if (!definition) throw new Error(`Unknown external storage provider: ${id}`)
    return definition
}

export function buildConnectionConfig(
    providerId: ExternalProviderId,
    values: Record<string, string>,
    platform: string,
): ExternalConnectionConfig {
    const definition = getExternalProviderDefinition(providerId)
    const location: Record<string, string> = {}
    for (const field of definition.fields) {
        if (field.location && values[field.key]) location[field.key] = values[field.key]
    }
    const projectId = values.projectId?.trim()
    const clientId = values.clientId?.trim()
    return {
        provider: providerId,
        ...(values.profile ? { profile: values.profile } : {}),
        endpoint: (values.endpoint || definition.defaultEndpoint).trim(),
        accountId: values.accountId?.trim() ?? '',
        location,
        ...(definition.oauth && projectId && clientId
            ? { oauthProfile: { projectId, platformClientIds: { [platform]: clientId } } }
            : {}),
    }
}

export function buildProviderSecret(
    providerId: ExternalProviderId,
    values: Record<string, string>,
): ExternalProviderSecretInput | null {
    switch (providerId) {
        case 'webdav': return { kind: 'webdav', password: values.password }
        case 's3': return { kind: 's3', accessKeyId: values.accessKeyId, secretAccessKey: values.secretAccessKey }
        case 'mybox': return { kind: 'mybox', pat: values.pat, expiresAtMs: String(new Date(values.expiresAtMs).getTime()) as `${number}` }
        case 'github_releases': return { kind: 'github', token: values.token }
        case 'gitlab_packages': return { kind: 'gitlab', token: values.token, tokenKind: values.tokenKind as 'deployToken' | 'personalAccessToken' | 'projectAccessToken' }
        case 'google_drive':
        case 'onedrive': return null
    }
}
