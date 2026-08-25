import type { LuaFactory } from 'wasmoon'

export function createLuaFactoryLoader(
    loadWasmoon: () => Promise<typeof import('wasmoon')>,
): () => Promise<LuaFactory> {
    let wasmoonModulePromise: Promise<typeof import('wasmoon')> | undefined

    return async () => {
        const pendingImport = (wasmoonModulePromise ??= loadWasmoon())
        let wasmoonModule: typeof import('wasmoon')
        try {
            wasmoonModule = await pendingImport
        } catch (error) {
            if (wasmoonModulePromise === pendingImport) {
                wasmoonModulePromise = undefined
            }
            throw error
        }

        const { LuaFactory } = wasmoonModule
        return new LuaFactory()
    }
}

export const createLuaFactory = createLuaFactoryLoader(() => import('wasmoon'))
