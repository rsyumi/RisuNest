export const legacyBackupIncludesInlays = false

export function isLegacyBackupAssetKey(key: string): boolean {
    const normalized = key.replace(/\\/g, '/')
    return normalized.startsWith('assets/') && normalized.length > 'assets/'.length
}

export function selectLegacyBackupAssetKeys(keys: readonly string[]): string[] {
    return keys.filter(isLegacyBackupAssetKey)
}
