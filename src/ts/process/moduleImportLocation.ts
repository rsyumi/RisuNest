import { v4 } from 'uuid'
import type { customscript, loreBook } from '../storage/database.svelte'
import type { NativeFileOperationImportLocation } from '../storage/nativeFileJobManager'
import type { RisuModule } from './modules'

export type ModuleImportLocation = Extract<NativeFileOperationImportLocation, { kind: 'module' | 'module-from-charx' | 'module-from-lorebook' }>

/** Where a native module import puts this file: a CharX is converted, a RISUM is added as it is. */
export function nativeModuleLocation(displayName: string): ModuleImportLocation {
    return /\.charx$/i.test(displayName) ? { kind: 'module-from-charx' } : { kind: 'module' }
}

/** The module a lorebook or regex JSON file imports as, with where it goes; null for other JSON. */
export async function convertJsonToModule(data: any): Promise<{ module: RisuModule, location: ModuleImportLocation } | null> {
    // data.type === 'risu' in conflict with HypaV3 preset exports
    // difference: record vs. array
    const [source, fields]: [string?, { lorebook?: loreBook[], regex?: customscript[] }?] =
        data.type === 'risu' && Array.isArray(data.data) ? ['risu lorebook', { lorebook: data.data }]
        : data.entries ? ['external lorebook', { lorebook: (await import('./lorebook.svelte')).convertExternalLorebook(data.entries) }]
        : data.type === 'regex' && data.data ? ['risu regex', { regex: data.data }]
        : []
    if (!source) return null
    return {
        module: {
            name: data.name || (fields.lorebook ? 'Imported Lorebook' : 'Imported Regex'),
            description: data.description || `Converted from ${source}`,
            ...fields,
            id: v4(),
        },
        location: { kind: fields.lorebook ? 'module-from-lorebook' : 'module' },
    }
}
