import type { LuaEngine, LuaFactory } from 'wasmoon'

export async function runLuaSource(
    engine: LuaEngine,
    source: string,
    timeoutMs = 2_000,
): Promise<void> {
    const thread = engine.global.newThread()
    const threadIndex = engine.global.getTop()

    try {
        thread.loadString(source)
        thread.setTimeout(Date.now() + timeoutMs)
        await thread.run()
    } finally {
        try {
            thread.close()
        } finally {
            engine.global.remove(threadIndex)
        }
    }
}

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
