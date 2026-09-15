import type {
    ExternalConnectionConfig,
    ExternalProviderId,
    ExternalProviderSecretInput,
    ExternalPublicationStrategy,
} from './types'

export interface ExternalProviderField {
    key: string
    label: string
    placeholder?: string
    required?: boolean
    secret?: boolean
    type?: 'text' | 'password' | 'select' | 'datetime-local'
    options?: Array<{ value: string; label: string }>
    location?: boolean
}

export interface ExternalProviderDefinition {
    id: ExternalProviderId
    name: string
    description: string
    defaultEndpoint: string
    profiles: Array<{ value: string; label: string }>
    fields: ExternalProviderField[]
    secretFields: ExternalProviderField[]
    oauth: boolean
    strategies: ExternalPublicationStrategy[]
    warning?: string
}

const s3Profiles = [
    { value: 'r2', label: 'Cloudflare R2' },
    { value: 'b2', label: 'Backblaze B2' },
    { value: 'hf', label: 'Hugging Face Storage Buckets' },
    { value: 'generic', label: 'Generic S3' },
]

export const externalProviderDefinitions: ExternalProviderDefinition[] = [
    {
        id: 'webdav', name: 'WebDAV / Koofr', oauth: false,
        description: 'Connect an HTTPS WebDAV folder with an application password.',
        defaultEndpoint: '', profiles: [{ value: '', label: 'Generic WebDAV' }, { value: 'koofr', label: 'Koofr' }],
        fields: [{ key: 'accountId', label: 'User name', required: true }, { key: 'root', label: 'Repository folder', required: true, location: true, placeholder: 'RisuNest' }],
        secretFields: [{ key: 'password', label: 'Application password', required: true, secret: true }],
        strategies: ['cas', 'sequential', 'backup-only'],
        warning: 'CAS is available only after this server passes a conditional-write probe.',
    },
    {
        id: 's3', name: 'S3 compatible', oauth: false,
        description: 'Use R2, B2, Hugging Face, or another S3-compatible bucket.',
        defaultEndpoint: '', profiles: s3Profiles,
        fields: [
            { key: 'accountId', label: 'Quota account name', required: true },
            { key: 'bucket', label: 'Bucket', required: true, location: true },
            { key: 'prefix', label: 'Prefix', location: true, placeholder: 'risunest' },
            { key: 'region', label: 'Region', location: true, placeholder: 'auto' },
            { key: 'addressing', label: 'Addressing', location: true, type: 'select', options: [{ value: '', label: 'Provider default' }, { value: 'path', label: 'Path' }, { value: 'virtual', label: 'Virtual host' }] },
        ],
        secretFields: [{ key: 'accessKeyId', label: 'Access key ID', required: true, secret: true }, { key: 'secretAccessKey', label: 'Secret access key', required: true, secret: true }],
        strategies: ['cas', 'sequential', 'backup-only'],
    },
    {
        id: 'google_drive', name: 'Google Drive', oauth: true,
        description: 'Connect a visible Drive folder or the app data space using OAuth.',
        defaultEndpoint: 'https://www.googleapis.com', profiles: [{ value: 'drive', label: 'Google Drive' }],
        fields: [
            { key: 'accountId', label: 'Permission ID', placeholder: 'Filled after sign-in' },
            { key: 'folderId', label: 'Folder ID', required: true, location: true },
            { key: 'space', label: 'Space', location: true, type: 'select', options: [{ value: 'drive', label: 'Visible Drive folder' }, { value: 'appDataFolder', label: 'Hidden app data' }] },
            { key: 'oauthRedirectUri', label: 'OAuth callback URL', required: true, location: true },
            { key: 'projectId', label: 'OAuth project ID', required: true },
            { key: 'clientId', label: 'OAuth client ID for this device', required: true },
        ], secretFields: [], strategies: ['sequential', 'backup-only'],
        warning: 'Google Drive synchronization is sequential. App data is removed when the application data is deleted.',
    },
    {
        id: 'onedrive', name: 'OneDrive', oauth: true,
        description: 'Connect a personal, business, or application folder through Microsoft Graph.',
        defaultEndpoint: 'https://graph.microsoft.com/v1.0', profiles: [],
        fields: [
            { key: 'accountId', label: 'Account identity', placeholder: 'Filled after sign-in' },
            { key: 'accountType', label: 'Account type', required: true, location: true, type: 'select', options: [{ value: 'personal', label: 'Personal' }, { value: 'business', label: 'Business' }, { value: 'appFolder', label: 'Application folder' }] },
            { key: 'tenant', label: 'Tenant', required: true, location: true, placeholder: 'common' },
            { key: 'driveId', label: 'Drive ID', required: true, location: true },
            { key: 'rootItemId', label: 'Root item ID', required: true, location: true },
            { key: 'redirectUri', label: 'Redirect URI', required: true, location: true },
            { key: 'projectId', label: 'Application client ID', required: true },
            { key: 'clientId', label: 'Platform client ID', required: true },
        ], secretFields: [], strategies: ['cas', 'sequential', 'backup-only'],
        warning: 'CAS remains unavailable until this connection passes a conditional-head probe.',
    },
    {
        id: 'mybox', name: 'NAVER MYBOX', oauth: false,
        description: 'Use a MYBOX personal access token and a dedicated folder.',
        defaultEndpoint: 'https://open-api.mybox.naver.com/v1',
        profiles: ['plan30gb', 'plan80gb', 'plan180gb', 'plan2tb', 'plan5tb', 'plan10tb', 'plan20tb'].map(value => ({ value, label: value.replace('plan', '') })),
        fields: [{ key: 'accountId', label: 'Account name', required: true }, { key: 'rootFolderName', label: 'Repository folder', required: true, location: true }, { key: 'rootFolderId', label: 'Existing folder resource ID', location: true }],
        secretFields: [{ key: 'pat', label: 'Personal access token', required: true, secret: true }, { key: 'expiresAtMs', label: 'Token expiry', required: true, type: 'datetime-local' }],
        strategies: ['sequential', 'backup-only'],
    },
    {
        id: 'github_releases', name: 'GitHub Releases', oauth: false,
        description: 'Append encrypted backups to draft releases in a private repository.',
        defaultEndpoint: 'https://api.github.com', profiles: [],
        fields: [{ key: 'accountId', label: 'GitHub login', required: true }, { key: 'uploadEndpoint', label: 'Upload endpoint', required: true, location: true, placeholder: 'https://uploads.github.com' }, { key: 'owner', label: 'Owner', required: true, location: true }, { key: 'repo', label: 'Private repository', required: true, location: true }, { key: 'tagPrefix', label: 'Tag prefix', required: true, location: true, placeholder: 'risunest-backup' }],
        secretFields: [{ key: 'token', label: 'Fine-grained personal access token', required: true, secret: true }],
        strategies: ['backup-only'], warning: 'This adapter creates draft releases and unique tags. Use a dedicated private repository.',
    },
    {
        id: 'gitlab_packages', name: 'GitLab Generic Packages', oauth: false,
        description: 'Append encrypted backups to a dedicated generic package namespace.',
        defaultEndpoint: 'https://gitlab.com', profiles: [{ value: '', label: 'Automatic' }, { value: 'gitlabCom', label: 'GitLab.com' }, { value: 'selfManaged', label: 'Self-managed' }],
        fields: [{ key: 'accountId', label: 'Token principal', required: true }, { key: 'projectId', label: 'Project ID or path', required: true, location: true }, { key: 'packageName', label: 'Package base name', required: true, location: true, placeholder: 'risunest-backup' }, { key: 'maxFileBytes', label: 'Maximum file bytes', location: true }],
        secretFields: [{ key: 'token', label: 'Access token', required: true, secret: true }, { key: 'tokenKind', label: 'Token kind', required: true, type: 'select', options: [{ value: 'personalAccessToken', label: 'Personal access token' }, { value: 'projectAccessToken', label: 'Project access token' }, { value: 'deployToken', label: 'Deploy token' }] }],
        strategies: ['backup-only'], warning: 'Package cleanup policies can remove backup data. This target does not synchronize.',
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
