import type { DataHealthFinding } from './dataHealth'

export interface DataHealthResolvedNames {
    ownerName?: string
    characterName?: string
    conversationName?: string
}

export interface DataHealthPresentationStrings {
    rootSeparatedField(field: string): string
    moduleAssetMissing(module: string, ordinal: number): string
    conversationModuleMissing(character: string, conversation: string): string
    conversationMessageInlayMissing(
        character: string,
        conversation: string,
        ordinal: number,
    ): string
}

export function dataHealthOwnerKey(kind: string, id: string): string {
    return JSON.stringify([kind, id])
}

export function describeDataHealthFinding(
    item: DataHealthFinding,
    names: DataHealthResolvedNames | null,
    strings: DataHealthPresentationStrings,
): string | null {
    if (item.code === 'record-invalid' && item.owner.kind === 'root') {
        const prefix = 'portable root contains separated field: '
        if (item.detail.startsWith(prefix)) {
            return strings.rootSeparatedField(item.detail.slice(prefix.length))
        }
    }
    const moduleAsset = /^\$\.assets\[(\d+)]\[1]$/.exec(
        item.locator?.sourcePath ?? '',
    )
    if (
        item.code === 'reference-invalid' &&
        item.owner.kind === 'module' &&
        item.target?.kind === 'asset' &&
        moduleAsset
    ) {
        return strings.moduleAssetMissing(
            names?.ownerName || item.owner.id,
            Number(moduleAsset[1]) + 1,
        )
    }
    if (item.owner.kind !== 'conversation') return null
    const separator = item.owner.id.indexOf('/')
    const characterId = separator < 0 ? item.owner.id : item.owner.id.slice(0, separator)
    const conversationId = separator < 0 ? item.owner.id : item.owner.id.slice(separator + 1)
    const character = names?.characterName || characterId
    const conversation = names?.conversationName || conversationId
    if (
        item.code === 'reference-missing' &&
        item.target?.kind === 'module' &&
        /^\$\.modules\[\d+]$/.test(item.locator?.sourcePath ?? '')
    ) {
        return strings.conversationModuleMissing(character, conversation)
    }
    const message = /^\$\.message\[(\d+)](?:\.data|\.swipes\[\d+])$/.exec(
        item.locator?.sourcePath ?? '',
    )
    if (
        item.code === 'reference-missing' &&
        item.target?.kind === 'inlay' &&
        message
    ) {
        return strings.conversationMessageInlayMissing(
            character,
            conversation,
            Number(message[1]) + 1,
        )
    }
    return null
}
