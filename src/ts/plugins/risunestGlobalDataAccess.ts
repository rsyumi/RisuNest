import type { RisunestHostContext } from './risunestHostContext'

// Named global variables and loadouts.

export function createRisunestGlobalDataAccess(context: RisunestHostContext) {
    void context
    const notImplemented = (method: string) => Promise.reject(new Error(`${method} is not implemented`))
    return {
        readGlobalVariables: (options?: unknown): Promise<unknown> => (void options, notImplemented('risunestReadGlobalVariables')),
        writeGlobalVariables: (set?: unknown): Promise<unknown> => (void set, notImplemented('risunestWriteGlobalVariables')),
        listLoadouts: (): Promise<unknown> => notImplemented('risunestListLoadouts'),
        writeLoadout: (loadout?: unknown): Promise<unknown> => (void loadout, notImplemented('risunestWriteLoadout')),
        deleteLoadout: (target?: unknown): Promise<unknown> => (void target, notImplemented('risunestDeleteLoadout')),
    }
}
