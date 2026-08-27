import type {
    NativeJpegAssetImportInput,
    NativeJpegAssetImportResult,
} from './nativeJpegAssetImport'

interface SelectedLegacyImage {
    name: string
    data: Uint8Array
}

export interface CharacterImageFileRouteDependencies {
    isTauriDesktop: boolean
    chooseDesktopPath(): Promise<string | null>
    readDesktopPath(path: string): Promise<Uint8Array>
    chooseLegacyFile(): Promise<SelectedLegacyImage | null>
    nativeImport(input: NativeJpegAssetImportInput): Promise<NativeJpegAssetImportResult>
    legacyImport(data: Uint8Array, name: string): Promise<string>
}

export type CharacterImageFileRouteResult =
    | { kind: 'cancelled' }
    | {
        kind: 'selected'
        logicalId: string
        bytes?: Uint8Array
        name?: string
    }

interface MutableCharacterImage {
    image?: string
    ccAssets?: Array<{
        type: string
        uri: string
        name: string
        ext: string
    }>
}

function characterImageExtension(uri: string): string {
    const path = uri.split(/[?#]/, 1)[0]
    const separator = path.lastIndexOf('.')
    const extension = separator < 0
        ? ''
        : path.slice(separator + 1).toLocaleLowerCase('en-US')
    return ['png', 'webp', 'gif', 'jpg', 'jpeg'].includes(extension)
        ? extension
        : 'png'
}

export function archiveCurrentCharacterImage(character: MutableCharacterImage): void {
    if (!character.image) return
    const uri = character.image
    character.ccAssets ??= []
    character.ccAssets.push({
        type: 'icon',
        name: 'iconx',
        uri,
        ext: characterImageExtension(uri),
    })
    character.image = ''
}

function fileNameFromPath(path: string): string {
    return path.split(/[\\/]/).at(-1) || path
}

function isJpegName(name: string): boolean {
    const extension = name.split('.').at(-1)?.toLocaleLowerCase('en-US')
    return extension === 'jpg' || extension === 'jpeg'
}

export async function selectCharacterImageFile(
    characterId: string,
    dependencies: CharacterImageFileRouteDependencies,
): Promise<CharacterImageFileRouteResult> {
    if (!dependencies.isTauriDesktop) {
        const selected = await dependencies.chooseLegacyFile()
        if (!selected) return { kind: 'cancelled' }
        return {
            kind: 'selected',
            logicalId: await dependencies.legacyImport(selected.data, selected.name),
            bytes: selected.data,
            name: selected.name,
        }
    }

    const path = await dependencies.chooseDesktopPath()
    if (!path) return { kind: 'cancelled' }
    const name = fileNameFromPath(path)
    if (isJpegName(name)) {
        const imported = await dependencies.nativeImport({
            source: { type: 'desktopPath', path },
            displayName: name,
            destination: { kind: 'current-character-image', characterId },
        })
        return { kind: 'selected', logicalId: imported.logicalId }
    }

    const bytes = await dependencies.readDesktopPath(path)
    return {
        kind: 'selected',
        logicalId: await dependencies.legacyImport(bytes, name),
        bytes,
        name,
    }
}
