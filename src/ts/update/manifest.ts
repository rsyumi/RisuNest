export type UpdateInstallStrategy = 'self-install' | 'stage-deb' | 'open-link' | 'disabled'
export type UpdatePackageFormat = 'zip' | 'nsis' | 'deb' | 'appimage' | 'dmg' | 'apk' | 'ipa' | 'tar.gz' | 'app.tar.gz'

export interface NativeUpdateEnvironment {
    currentVersion: string
    installStrategy: UpdateInstallStrategy
    configured: boolean
    disabledReason: 'agent-build' | 'not-configured' | 'invalid-configuration' | null
}

export interface AvailableAppUpdate {
    handleId: string
    version: string
    pubDate: string
    notes: string
    localizedNotes: Record<string, string>
    releasePage: string
    installStrategy: UpdateInstallStrategy
    downloadUrl: string
    downloadSize: number
    format: UpdatePackageFormat | null
}

export interface NativeUpdateCheckResult {
    status: 'available' | 'current' | 'unsupported' | 'disabled'
    currentVersion: string
    disabledReason: NativeUpdateEnvironment['disabledReason']
    update: AvailableAppUpdate | null
}

export interface NativeUpdateProgress {
    handleId: string
    downloaded: number
    total: number | null
}

export interface StagedDebUpdate {
    path: string
    installCommand: string
}

export function compareSemver(left: string, right: string): number {
    const parse = (value: string): number[] => {
        if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(value)) {
            throw new Error(`Invalid stable version: ${value}`)
        }
        return value.split('.').map(Number)
    }
    const a = parse(left)
    const b = parse(right)
    for (let index = 0; index < 3; index += 1) {
        if (a[index] !== b[index]) return a[index] > b[index] ? 1 : -1
    }
    return 0
}
