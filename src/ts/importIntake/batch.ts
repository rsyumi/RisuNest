import { parseImportJson } from '../importUtf8'
import { importContentBatch } from '../storage/contentImportBatch'
import type { NativeFileOperationSource, SharedNativeFileOperationContext } from '../storage/nativeFileJobManager'
import { NativeFileJobError } from '../storage/nativeFileJobs'
import { importSourceName, nativeImportSource, readImportSourceBytes } from './sources'
import type { ImportIntakeItem, ImportSheetRowDecision } from './types'

/** The destination chosen for one file: the sheet's choice, or the default of a direct import. */
export type ImportIntakeDecision = Omit<ImportSheetRowDecision, 'id'>

export interface ImportIntakeRow {
    item: ImportIntakeItem
    decision: ImportIntakeDecision
}

export function describeIntakeItem(item: ImportIntakeItem): NativeFileOperationSource {
    return item.bytes === undefined ? { name: item.name } : { name: item.name, bytes: item.bytes }
}

/** Imports the chosen files one after another inside the caller's operation. */
export function importIntakeRows(
    context: SharedNativeFileOperationContext,
    rows: readonly ImportIntakeRow[],
): Promise<string | null> {
    return importContentBatch(
        context,
        rows,
        row => describeIntakeItem(row.item),
        row => importIntakeItem(row.item, row.decision, context),
    )
}

// The TypeScript converters pick their branch by extension; content decided the kind already.
function jsonImportName(name: string): string {
    return /\.json$/i.test(name) ? name.replace(/\.json$/i, '.json') : `${name}.json`
}

function characterImportName(item: ImportIntakeItem): string {
    switch (item.format) {
        case 'png':
            return /\.png$/i.test(item.name) ? item.name : `${item.name}.png`
        case 'charx':
            return /\.charx$/i.test(item.name) ? item.name : `${item.name}.charx`
        case 'jpeg-charx':
            return /\.jpe?g$/i.test(item.name) ? item.name : `${item.name}.jpeg`
        default:
            return jsonImportName(item.name)
    }
}

function presetImportName(item: ImportIntakeItem): string {
    if (item.format === 'risup' || item.format === 'risupreset') {
        const extension = `.${item.format}`
        return item.name.toLowerCase().endsWith(extension) ? item.name.slice(0, -extension.length) + extension : item.name + extension
    }
    return jsonImportName(item.name)
}

function nativeOptions(context: SharedNativeFileOperationContext) {
    return { signal: context.signal, onStatus: context.onStatus }
}

/**
 * Imports one classified file inside the caller's operation and records where it went. Resolves
 * null when the importer declined it, for example after its own refusal alert.
 */
export async function importIntakeItem(
    item: ImportIntakeItem,
    decision: ImportIntakeDecision,
    context: SharedNativeFileOperationContext,
): Promise<string | null> {
    context.signal.throwIfAborted()
    switch (item.kind) {
        case 'character':
            return item.format === 'charx' && decision.charxAs === 'module'
                ? importNativeModule(item, context)
                : importCharacter(item, context)
        case 'module':
            return item.format === 'json'
                ? importModuleBytes(item, context)
                : importNativeModule(item, context)
        case 'lorebook': {
            const target = decision.lorebookTarget ?? { type: 'new-module' }
            if (target.type === 'new-module') return importModuleBytes(item, context)
            const { appendCharacterLorebook, lorebookFileEntries } = await import('../process/lorebook.svelte')
            const data = await readImportSourceBytes(item.source, context.signal)
            // An empty lorebook imports as one, as it does into a new module.
            if (!await appendCharacterLorebook(target.chaId, lorebookFileEntries(parseImportJson(data).value))) {
                throw new NativeFileJobError('revision-conflict', `${target.name} was removed during the import`)
            }
            context.setLocation({ kind: 'character-lorebook', chaId: target.chaId, characterName: target.name })
            return target.chaId
        }
        case 'preset': {
            const { importPreset } = await import('../storage/database.svelte')
            const data = await readImportSourceBytes(item.source, context.signal)
            const index = await importPreset({ name: presetImportName(item), data })
            if (typeof index !== 'number') return null
            context.setLocation({ kind: 'preset' })
            return `preset:${index}`
        }
        default:
            throw new Error(`${importSourceName(item.source)} cannot be imported`)
    }
}

async function importCharacter(
    item: ImportIntakeItem,
    context: SharedNativeFileOperationContext,
): Promise<string | null> {
    const source = nativeImportSource(item.source)
    if (source?.type === 'desktopPath') {
        const { importCharacterPathInOperation } = await import('../characterCards')
        return importCharacterPathInOperation(source.path, context, characterImportName(item))
    }
    if (!source) throw new Error(`${item.name} cannot be imported`)
    const [{ importCharacterProcess, importPreparedNativeCharacterContent }, { getDatabase }, { checkCharOrder }] = await Promise.all([
        import('../characterCards'),
        import('../storage/database.svelte'),
        import('../globalApi.svelte'),
    ])
    let characterId: string | null
    if (item.format === 'json') {
        // JSON spools keep the JavaScript converter, as the Android content picker does.
        const data = await readImportSourceBytes(item.source, context.signal)
        const index = await importCharacterProcess({ name: characterImportName(item), data, quiet: true, signal: context.signal })
        characterId = typeof index === 'number' ? getDatabase().characters[index]?.chaId ?? null : null
    } else {
        // The native importer reads the format from the content, and an Android spool is claimed only under the name it recorded.
        const result = await importPreparedNativeCharacterContent({ source, displayName: item.name }, nativeOptions(context))
        characterId = result.kind === 'imported' ? result.value : null
    }
    if (!characterId) return null
    checkCharOrder()
    context.setLocation({ kind: 'character', chaId: characterId })
    return characterId
}

async function importNativeModule(
    item: ImportIntakeItem,
    context: SharedNativeFileOperationContext,
): Promise<string | null> {
    const source = nativeImportSource(item.source)
    if (!source) throw new Error(`${item.name} cannot be imported`)
    const { importPreparedNativeModuleContent } = await import('../process/modules')
    const result = await importPreparedNativeModuleContent({ source, displayName: item.name }, nativeOptions(context))
    if (result.kind !== 'imported') return null
    context.setLocation(item.format === 'charx' ? { kind: 'module-from-charx' } : { kind: 'module' })
    return result.value
}

async function importModuleBytes(
    item: ImportIntakeItem,
    context: SharedNativeFileOperationContext,
): Promise<string | null> {
    const { importModuleData } = await import('../process/modules')
    const data = await readImportSourceBytes(item.source, context.signal)
    const location = await importModuleData({ name: jsonImportName(item.name), data, signal: context.signal })
    if (!location) return null
    context.setLocation(location)
    return location.kind
}
