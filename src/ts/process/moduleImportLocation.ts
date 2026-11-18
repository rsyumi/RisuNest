import type { NativeFileOperationImportLocation } from '../storage/nativeFileJobManager'

export type ModuleImportLocation = Extract<NativeFileOperationImportLocation, { kind: 'module' | 'module-from-charx' | 'module-from-lorebook' }>

/** Where a native module import puts this file: a CharX is converted, a RISUM is added as it is. */
export function nativeModuleLocation(displayName: string): ModuleImportLocation {
    return /\.charx$/i.test(displayName) ? { kind: 'module-from-charx' } : { kind: 'module' }
}
