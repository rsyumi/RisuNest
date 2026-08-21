export type AssetSourceRoute = 'account' | 'tauri-asset' | 'tauri-path' | 'web-local'

export function selectAssetSourceRoute(
    location: string,
    isTauri: boolean,
    isAccount: boolean,
): AssetSourceRoute {
    if (isAccount && location.startsWith('assets')) return 'account'
    if (!isTauri) return 'web-local'
    return location.startsWith('assets') ? 'tauri-asset' : 'tauri-path'
}
